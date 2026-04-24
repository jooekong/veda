use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::stream::{self, StreamExt};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn};

use veda_core::store::{EmbeddingService, LlmService, MetadataStore, TaskQueue, VectorStore};
use veda_pipeline::chunking::semantic_chunk;
use veda_pipeline::summary;
use veda_types::*;

pub struct Worker {
    meta: Arc<dyn MetadataStore>,
    task_queue: Arc<dyn TaskQueue>,
    vector: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    llm: Option<Arc<dyn LlmService>>,
    batch_size: usize,
    poll_interval: Duration,
}

impl Worker {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        task_queue: Arc<dyn TaskQueue>,
        vector: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingService>,
        llm: Option<Arc<dyn LlmService>>,
        batch_size: usize,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            meta,
            task_queue,
            vector,
            embedding,
            llm,
            batch_size,
            poll_interval: Duration::from_secs(poll_interval_secs),
        }
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        info!("worker started");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("worker shutting down");
                    break;
                }
                _ = self.poll_once() => {}
            }
        }
    }

    async fn poll_once(&self) {
        match self.task_queue.claim(self.batch_size).await {
            Ok(tasks) if tasks.is_empty() => {
                sleep(self.poll_interval).await;
            }
            Ok(tasks) => {
                let concurrency = self.batch_size.max(1);
                stream::iter(tasks)
                    .for_each_concurrent(concurrency, |task| async move {
                        if let Err(e) = self.process_task(&task).await {
                            error!(task_id = task.id, err = %e, "task failed");
                            let _ = self.task_queue.fail(task.id, &e.to_string()).await;
                        }
                    })
                    .await;
            }
            Err(e) => {
                warn!(err = %e, "claim failed");
                sleep(self.poll_interval).await;
            }
        }
    }

    async fn process_task(&self, task: &OutboxEvent) -> veda_types::Result<()> {
        let file_id = task
            .payload
            .get("file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match task.event_type {
            OutboxEventType::ChunkSync => {
                self.handle_chunk_sync(&task.workspace_id, file_id).await?;
                self.task_queue.complete(task.id).await?;
                if self.llm.is_some() {
                    self.enqueue_summary_sync(&task.workspace_id, file_id)
                        .await?;
                }
            }
            OutboxEventType::ChunkDelete => {
                self.vector
                    .delete_chunks(&task.workspace_id, file_id)
                    .await?;
                self.vector
                    .delete_summary(&task.workspace_id, file_id)
                    .await?;
                self.meta.delete_summary_by_file(file_id).await?;
                self.task_queue.complete(task.id).await?;
            }
            OutboxEventType::SummarySync => {
                self.handle_summary_sync(&task.workspace_id, file_id)
                    .await?;
                self.task_queue.complete(task.id).await?;
            }
            OutboxEventType::DirSummarySync => {
                let dentry_id = task
                    .payload
                    .get("dentry_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let parent_path = task
                    .payload
                    .get("parent_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.handle_dir_summary_sync(&task.workspace_id, dentry_id, parent_path)
                    .await?;
                self.task_queue.complete(task.id).await?;
            }
            OutboxEventType::CollectionSync => {
                let msg = "CollectionSync not implemented";
                warn!(task_id = task.id, msg);
                return Err(VedaError::Internal(msg.to_string()));
            }
        }
        Ok(())
    }

    async fn handle_chunk_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        let Some(file) = self.meta.get_file(file_id).await? else {
            warn!(
                workspace_id,
                file_id, "chunk_sync skipped: file no longer exists"
            );
            self.vector.delete_chunks(workspace_id, file_id).await?;
            return Ok(());
        };

        let content = match file.storage_type {
            StorageType::Inline => self
                .meta
                .get_file_content(file_id)
                .await?
                .unwrap_or_default(),
            StorageType::Chunked => {
                let chunks = self.meta.get_file_chunks(file_id, None, None).await?;
                let total: usize = chunks.iter().map(|c| c.content.len()).sum();
                let mut buf = String::with_capacity(total);
                for c in chunks {
                    buf.push_str(&c.content);
                }
                buf
            }
        };

        let sem_chunks = semantic_chunk(&content, 2048);
        if sem_chunks.is_empty() {
            self.vector.delete_chunks(workspace_id, file_id).await?;
            return Ok(());
        }

        let texts: Vec<String> = sem_chunks.iter().map(|c| c.content.clone()).collect();
        let embeddings = self.embedding.embed(&texts).await?;

        let chunks_with_emb: Vec<ChunkWithEmbedding> = sem_chunks
            .into_iter()
            .zip(embeddings)
            .map(|(chunk, vector)| ChunkWithEmbedding {
                id: format!("{file_id}_{}", chunk.index),
                workspace_id: workspace_id.to_string(),
                file_id: file_id.to_string(),
                chunk_index: chunk.index,
                content: chunk.content,
                vector,
            })
            .collect();

        self.vector.upsert_chunks(&chunks_with_emb).await?;
        Ok(())
    }

    async fn enqueue_summary_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        let now = Utc::now();
        let event = OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: OutboxEventType::SummarySync,
            payload: serde_json::json!({"file_id": file_id}),
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at: now,
            lease_until: None,
            created_at: now,
        };
        self.task_queue.enqueue(&event).await?;
        Ok(())
    }

    async fn handle_summary_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        let Some(llm) = &self.llm else {
            return Ok(());
        };

        let Some(file) = self.meta.get_file(file_id).await? else {
            warn!(file_id, "summary_sync skipped: file no longer exists");
            return Ok(());
        };

        let content = match file.storage_type {
            StorageType::Inline => self
                .meta
                .get_file_content(file_id)
                .await?
                .unwrap_or_default(),
            StorageType::Chunked => {
                let chunks = self.meta.get_file_chunks(file_id, None, None).await?;
                chunks.into_iter().map(|c| c.content).collect::<String>()
            }
        };

        if content.trim().is_empty() {
            return Ok(());
        }

        let l0 = summary::generate_l0(llm.as_ref(), &content).await?;
        let l1 = summary::generate_l1(llm.as_ref(), &content).await?;

        let summary_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let file_summary = FileSummary {
            id: summary_id.clone(),
            workspace_id: workspace_id.to_string(),
            file_id: Some(file_id.to_string()),
            dentry_id: None,
            l0_abstract: l0.clone(),
            l1_overview: l1,
            status: SummaryStatus::Ready,
            created_at: now,
            updated_at: now,
        };
        self.meta.upsert_summary(&file_summary).await?;

        let embeddings = self.embedding.embed(&[l0.clone()]).await?;
        if let Some(vector) = embeddings.into_iter().next() {
            let summary_emb = SummaryWithEmbedding {
                id: file_id.to_string(),
                workspace_id: workspace_id.to_string(),
                summary_type: "file".to_string(),
                content: l0,
                vector,
            };
            self.vector.upsert_summaries(&[summary_emb]).await?;
        }

        // Trigger parent directory summary aggregation
        if let Some(path) = self
            .meta
            .get_dentry_path_by_file_id(workspace_id, file_id)
            .await?
        {
            let parent = parent_path_of(&path);
            if let Some(parent_dentry) = self.meta.get_dentry(workspace_id, &parent).await? {
                self.enqueue_dir_summary_sync(workspace_id, &parent_dentry.id, &parent)
                    .await?;
            }
        }

        Ok(())
    }

    async fn enqueue_dir_summary_sync(
        &self,
        workspace_id: &str,
        dentry_id: &str,
        parent_path: &str,
    ) -> veda_types::Result<()> {
        let now = Utc::now();
        let event = OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: OutboxEventType::DirSummarySync,
            payload: serde_json::json!({
                "dentry_id": dentry_id,
                "parent_path": parent_path,
            }),
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at: now,
            lease_until: None,
            created_at: now,
        };
        self.task_queue.enqueue(&event).await?;
        Ok(())
    }

    async fn handle_dir_summary_sync(
        &self,
        workspace_id: &str,
        dentry_id: &str,
        dir_path: &str,
    ) -> veda_types::Result<()> {
        let Some(llm) = &self.llm else {
            return Ok(());
        };

        let child_summaries = self.meta.list_child_summaries(workspace_id, dir_path).await?;
        if child_summaries.is_empty() {
            return Ok(());
        }

        let child_l0s: Vec<String> = child_summaries.iter().map(|s| s.l0_abstract.clone()).collect();
        let (l0, l1) = summary::aggregate_dir_summary(llm.as_ref(), &child_l0s).await?;

        let summary_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let dir_summary = FileSummary {
            id: summary_id,
            workspace_id: workspace_id.to_string(),
            file_id: None,
            dentry_id: Some(dentry_id.to_string()),
            l0_abstract: l0.clone(),
            l1_overview: l1,
            status: SummaryStatus::Ready,
            created_at: now,
            updated_at: now,
        };
        self.meta.upsert_summary(&dir_summary).await?;

        // Recurse: trigger parent directory aggregation (stop at root)
        let parent = parent_path_of(dir_path);
        if parent != dir_path {
            if let Some(parent_dentry) = self.meta.get_dentry(workspace_id, &parent).await? {
                self.enqueue_dir_summary_sync(workspace_id, &parent_dentry.id, &parent)
                    .await?;
            }
        }

        Ok(())
    }
}

fn parent_path_of(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => "/".to_string(),
    }
}

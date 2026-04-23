use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn};

use veda_core::store::{EmbeddingService, MetadataStore, TaskQueue, VectorStore};
use veda_pipeline::chunking::semantic_chunk;
use veda_types::*;

pub struct Worker {
    meta: Arc<dyn MetadataStore>,
    task_queue: Arc<dyn TaskQueue>,
    vector: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    batch_size: usize,
    poll_interval: Duration,
}

impl Worker {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        task_queue: Arc<dyn TaskQueue>,
        vector: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingService>,
        batch_size: usize,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            meta,
            task_queue,
            vector,
            embedding,
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
            }
            OutboxEventType::ChunkDelete => {
                self.vector
                    .delete_chunks(&task.workspace_id, file_id)
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

    async fn handle_chunk_sync(&self, workspace_id: &str, file_id: &str) -> veda_types::Result<()> {
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

        // upsert_chunks handles insert-then-cleanup: new data is persisted first,
        // stale chunks (from previous version with more chunks) are cleaned after.
        self.vector.upsert_chunks(&chunks_with_emb).await?;
        Ok(())
    }
}

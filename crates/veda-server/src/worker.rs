use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::stream::{self, StreamExt};
use futures::FutureExt;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use veda_core::store::{EmbeddingService, LlmService, MetadataStore, TaskQueue, VectorStore};
use veda_pipeline::chunking::semantic_chunk;
use veda_pipeline::summary;
use veda_types::*;

use crate::outbox::enqueue_dedup;

/// Delay before a SummarySync / DirSummarySync becomes claimable when the
/// edit appears to be part of a burst. Pairs with has_pending_event dedup to
/// coalesce repeated edits within the window into a single regeneration.
const SUMMARY_DEBOUNCE_SECS: i64 = 30;

/// If the most recent existing summary for a file/dir is older than this,
/// the current edit is treated as isolated (not part of a burst) and the
/// task runs immediately. Without this, even a single edit on a long-quiet
/// file would wait the full debounce delay before showing fresh summary.
const SUMMARY_BURST_WINDOW_SECS: i64 = 5 * 60;

pub struct Worker {
    meta: Arc<dyn MetadataStore>,
    task_queue: Arc<dyn TaskQueue>,
    vector: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    llm: Option<Arc<dyn LlmService>>,
    batch_size: usize,
    poll_interval: Duration,
    max_overview_tokens: usize,
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
        max_overview_tokens: usize,
    ) -> Self {
        Self {
            meta,
            task_queue,
            vector,
            embedding,
            llm,
            batch_size,
            poll_interval: Duration::from_secs(poll_interval_secs),
            max_overview_tokens,
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
                        let event_type = outbox_event_label(task.event_type);
                        let started = std::time::Instant::now();
                        // catch_unwind so a panic in one task (e.g. a malformed
                        // payload triggering an unreachable! deeper down) marks
                        // that task as failed instead of killing the worker
                        // future and leaving lease_until-stuck rows.
                        let result = AssertUnwindSafe(self.process_task(&task))
                            .catch_unwind()
                            .await;
                        let elapsed = started.elapsed().as_secs_f64();
                        ::metrics::histogram!(
                            "veda_outbox_process_seconds",
                            "event_type" => event_type,
                        )
                        .record(elapsed);
                        let task_outcome = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.to_string()),
                            Err(panic) => {
                                let msg = panic_message(&panic);
                                error!(task_id = task.id, %msg, "task panicked");
                                Some(format!("panic: {msg}"))
                            }
                        };
                        if let Some(err_msg) = task_outcome {
                            error!(task_id = task.id, err = %err_msg, "task failed");
                            ::metrics::counter!(
                                "veda_outbox_failed_total",
                                "event_type" => event_type,
                            )
                            .increment(1);
                            let _ = self.task_queue.fail(task.id, &err_msg).await;
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
        // force_reembed=true bypasses the W1.3 last_embedded_content_hash
        // short-circuit. The reconciler sets it when MySQL has a file but
        // Milvus is missing chunks for it — the watermark would otherwise
        // (correctly, from MySQL's PoV) say "already embedded" and the
        // worker would silently no-op despite the actual data loss.
        let force_reembed = task
            .payload
            .get("force_reembed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match task.event_type {
            OutboxEventType::ChunkSync => {
                self.handle_chunk_sync(&task.workspace_id, file_id, force_reembed)
                    .await?;
                // Enqueue summary BEFORE completing the ChunkSync, so a
                // failure in summary enqueue retries the whole ChunkSync.
                // The retry's handle_chunk_sync short-circuits via
                // last_embedded_content_hash (no wasted embed), and
                // enqueue_summary_sync has its own has_pending_event guard.
                if self.llm.is_some() {
                    self.enqueue_summary_sync(&task.workspace_id, file_id)
                        .await?;
                } else {
                    // [llm] not configured: a previous summary (generated
                    // when LLM was enabled) would otherwise persist and
                    // serve stale content for an updated file. Delete the
                    // metadata row first so get_summary returns 501
                    // immediately; the Milvus side can lag without users
                    // ever seeing stale L0/L1.
                    self.meta.delete_summary_by_file(file_id).await?;
                    self.vector
                        .delete_summary(&task.workspace_id, file_id)
                        .await?;
                }
                self.task_queue.complete(task.id).await?;
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
        }
        Ok(())
    }

    /// Load a file's full content into a single `String`, paginating chunked
    /// storage in batches of 32 chunks. Caps peak memory at ~file_size + one
    /// batch (the alternative — `get_file_chunks(None, None)` — holds the
    /// full `Vec<FileChunk>` plus the assembled buffer simultaneously, ~2x).
    async fn load_full_content(&self, file: &FileRecord) -> veda_types::Result<String> {
        match file.storage_type {
            StorageType::Inline => Ok(self
                .meta
                .get_file_content(&file.id)
                .await?
                .unwrap_or_default()),
            StorageType::Chunked => {
                let total = file.size_bytes.max(0) as usize;
                let mut buf = String::with_capacity(total);
                let byte_lens = self.meta.list_chunk_byte_lens(&file.id).await?;
                if let (Some(first), Some(last)) = (byte_lens.first(), byte_lens.last()) {
                    const CHUNK_BATCH: i32 = 32;
                    let mut lo = first.0;
                    let last_idx = last.0;
                    while lo <= last_idx {
                        let hi = lo.saturating_add(CHUNK_BATCH - 1).min(last_idx);
                        let batch = self
                            .meta
                            .get_chunks_in_index_range(&file.id, lo, hi)
                            .await?;
                        for c in batch {
                            buf.push_str(&c.content);
                        }
                        lo = hi.saturating_add(1);
                    }
                }
                Ok(buf)
            }
        }
    }

    async fn handle_chunk_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
        force_reembed: bool,
    ) -> veda_types::Result<()> {
        let Some(file) = self.meta.get_file(file_id).await? else {
            warn!(
                workspace_id,
                file_id, "chunk_sync skipped: file no longer exists"
            );
            self.vector.delete_chunks(workspace_id, file_id).await?;
            return Ok(());
        };

        // Skip the entire embed pipeline if the content has not changed since
        // the last successful Milvus upsert. The watermark below is written at
        // the end of this function, so a worker crash between upsert and
        // watermark write will redo the work (idempotent), but a successful
        // run followed by an outbox replay (lease expired, claim retry, etc.)
        // will short-circuit here.
        //
        // The reconciler-driven path sets `force_reembed=true` when it sees
        // Milvus missing chunks for a file MySQL says is already embedded —
        // in that case the watermark is correct relative to past embed
        // history but Milvus has actually lost the data, so we must bypass.
        if !force_reembed
            && file.last_embedded_content_hash.as_deref() == Some(file.checksum_sha256.as_str())
        {
            debug!(file_id, "chunk_sync skipped: content unchanged since last embed");
            return Ok(());
        }

        let content = self.load_full_content(&file).await?;

        let sem_chunks = semantic_chunk(&content, 2048);
        // Drop the assembled file buffer before the embed loop allocates per-batch
        // text clones. semantic_chunk has already copied the bytes it needs.
        drop(content);
        if sem_chunks.is_empty() {
            self.vector.delete_chunks(workspace_id, file_id).await?;
            // Empty content is "embedded as nothing"; record the watermark so
            // re-embedding the same empty file short-circuits.
            self.meta
                .update_file_content_hash(file_id, &file.checksum_sha256)
                .await?;
            return Ok(());
        }

        // Embed + upsert in batches so memory peaks at one batch's worth of
        // texts/embeddings/ChunkWithEmbedding rather than the entire file.
        // Upserts are idempotent (vector id keyed on file_id+chunk_index), so
        // a mid-loop failure is safe — the next outbox claim retries from the
        // first batch. The watermark below only fires after every batch lands.
        //
        // Critical: use `upsert_chunks_only` per batch, then ONE
        // `delete_chunks_above` after every batch succeeds. The all-in-one
        // `upsert_chunks` would delete chunk_index > batch_max after batch 1,
        // wiping any stale tail chunks before batch 2 had a chance to write
        // them — if batch 2+ failed the search index would be missing the
        // tail until a successful retry. Splitting the sweep keeps stale
        // chunks alive (briefly co-existing with new ones, both keyed on
        // file_id+chunk_index so duplicates are impossible) until we know
        // the full new set landed.
        const EMBED_BATCH: usize = 64;
        let max_chunk_index = sem_chunks
            .last()
            .map(|c| c.index)
            .expect("sem_chunks empty handled above");
        for batch in sem_chunks.chunks(EMBED_BATCH) {
            let texts: Vec<String> = batch.iter().map(|c| c.content.clone()).collect();
            let embeddings = self.embedding.embed(&texts).await?;
            let with_emb: Vec<ChunkWithEmbedding> = batch
                .iter()
                .zip(embeddings)
                .map(|(chunk, vector)| ChunkWithEmbedding {
                    id: format!("{file_id}_{}", chunk.index),
                    workspace_id: workspace_id.to_string(),
                    file_id: file_id.to_string(),
                    chunk_index: chunk.index,
                    content: chunk.content.clone(),
                    vector,
                })
                .collect();
            self.vector.upsert_chunks_only(&with_emb).await?;
        }
        self.vector
            .delete_chunks_above(workspace_id, file_id, max_chunk_index)
            .await?;
        // Watermark only after Milvus upsert succeeds. If this update fails,
        // the next claim retries embed+upsert (wasted) but data stays correct.
        self.meta
            .update_file_content_hash(file_id, &file.checksum_sha256)
            .await?;
        Ok(())
    }

    async fn enqueue_summary_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        // Burst detection uses `summary.updated_at` (when the previous LLM
        // run finished) — not file.updated_at. Trade-off: the very first
        // edit after a long quiet period generates a summary immediately
        // (no debounce, since no prior summary exists), and the second
        // edit within ~5min sees that fresh summary and gets debounced.
        // A back-to-back burst on a never-summarized file produces 2 LLM
        // calls, not 1; subsequent bursts coalesce as expected. The
        // has_pending_event dedup in enqueue_dedup already collapses
        // repeats arriving while a task is queued — the only leak is the
        // first edit of a brand-new file, accepted.
        let now = Utc::now();
        let in_burst = self
            .meta
            .get_summary_by_file(file_id)
            .await?
            .map(|s| (now - s.updated_at).num_seconds() < SUMMARY_BURST_WINDOW_SECS)
            .unwrap_or(false);
        let available_at = if in_burst {
            now + chrono::Duration::seconds(SUMMARY_DEBOUNCE_SECS)
        } else {
            now
        };
        let inserted = enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            OutboxEventType::SummarySync,
            "file_id",
            file_id,
            serde_json::json!({"file_id": file_id}),
            available_at,
        )
        .await?;
        if !inserted {
            info!(file_id, "summary_sync already pending, skipping enqueue");
            return Ok(());
        }
        let debounce_secs = if in_burst { SUMMARY_DEBOUNCE_SECS } else { 0 };
        debug!(file_id, in_burst, debounce_secs, "enqueued summary_sync");
        ::metrics::counter!(
            "veda_summary_enqueue_total",
            "burst" => if in_burst { "in_burst" } else { "isolated" },
        )
        .increment(1);
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

        let content = self.load_full_content(&file).await?;

        if content.trim().is_empty() {
            return Ok(());
        }

        let max_tokens = self.max_overview_tokens;
        let (l0, l1) = tokio::try_join!(
            summary::generate_l0(llm.as_ref(), &content),
            summary::generate_l1(llm.as_ref(), &content, max_tokens),
        )?;

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
        let in_burst = self
            .meta
            .get_summary_by_dentry(dentry_id)
            .await?
            .map(|s| (now - s.updated_at).num_seconds() < SUMMARY_BURST_WINDOW_SECS)
            .unwrap_or(false);
        let available_at = if in_burst {
            now + chrono::Duration::seconds(SUMMARY_DEBOUNCE_SECS)
        } else {
            now
        };
        let inserted = enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            OutboxEventType::DirSummarySync,
            "dentry_id",
            dentry_id,
            serde_json::json!({
                "dentry_id": dentry_id,
                "parent_path": parent_path,
            }),
            available_at,
        )
        .await?;
        if !inserted {
            info!(dentry_id, "dir_summary_sync already pending, skipping enqueue");
        }
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

        let child_summaries = self
            .meta
            .list_child_summaries(workspace_id, dir_path)
            .await?;
        if child_summaries.is_empty() {
            return Ok(());
        }

        let child_l0s: Vec<String> = child_summaries
            .iter()
            .map(|s| s.l0_abstract.clone())
            .collect();
        let max_tokens = self.max_overview_tokens;
        let (l0, l1) = summary::aggregate_dir_summary(llm.as_ref(), &child_l0s, max_tokens).await?;

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

        // Broadcast a SummaryReady fs_event so FUSE clients can drop their
        // `(dir, .abstract/.overview)` miss-cache entry immediately instead
        // of waiting the full attr_ttl (~5s) for the next probe. Failure
        // here is non-fatal — clients still recover via TTL expiry, so we
        // log and continue rather than rolling back the summary write.
        let summary_event = FsEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: FsEventType::SummaryReady,
            path: dir_path.to_string(),
            file_id: None,
            created_at: now,
        };
        if let Err(e) = self.meta.insert_fs_event_direct(&summary_event).await {
            warn!(dir_path, err = %e, "summary_ready event emit failed (sidecar miss cache will fall back to TTL)");
        }

        let embeddings = self.embedding.embed(&[l0.clone()]).await?;
        if let Some(vector) = embeddings.into_iter().next() {
            let summary_emb = SummaryWithEmbedding {
                id: dentry_id.to_string(),
                workspace_id: workspace_id.to_string(),
                summary_type: "dir".to_string(),
                content: l0,
                vector,
            };
            self.vector.upsert_summaries(&[summary_emb]).await?;
        }

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

/// Static label string for `OutboxEventType`, used as a Prometheus label.
/// Must be `&'static str` because `metrics` labels need it.
fn outbox_event_label(event: OutboxEventType) -> &'static str {
    match event {
        OutboxEventType::ChunkSync => "chunk_sync",
        OutboxEventType::ChunkDelete => "chunk_delete",
        OutboxEventType::SummarySync => "summary_sync",
        OutboxEventType::DirSummarySync => "dir_summary_sync",
    }
}

/// Best-effort extraction of a panic message from `catch_unwind`'s payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;

    /// Drive a future that panics through `catch_unwind` and feed the payload
    /// into `panic_message`. This exercises the exact path `Worker::run` takes
    /// when a task handler panics, without spinning up a real worker.
    async fn extract(panic: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let result =
            AssertUnwindSafe(async move { panic() }).catch_unwind().await;
        let payload = result.expect_err("panic should be caught");
        panic_message(&payload)
    }

    #[tokio::test]
    async fn extracts_static_str_panic() {
        let msg = extract(|| panic!("static panic msg")).await;
        assert_eq!(msg, "static panic msg");
    }

    #[tokio::test]
    async fn extracts_owned_string_panic() {
        let dynamic = String::from("owned panic ").repeat(2) + "msg";
        let msg = extract(move || panic!("{}", dynamic)).await;
        assert_eq!(msg, "owned panic owned panic msg");
    }

    #[tokio::test]
    async fn falls_back_for_non_string_payload() {
        // panic!(123_i32) sends an i32 — neither &str nor String. The fallback
        // string keeps the worker from crashing on exotic payloads.
        let msg = extract(|| std::panic::panic_any(123_i32)).await;
        assert_eq!(msg, "(non-string panic payload)");
    }
}

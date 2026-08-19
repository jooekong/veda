use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::stream::{self, StreamExt};
use futures::FutureExt;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use veda_core::store::{
    EmbeddingService, LlmService, MemoryStore, MemoryVectorStore, MetadataStore, TaskQueue,
    VectorStore,
};
use veda_pipeline::chunking::{is_binary_content, semantic_chunk};
use veda_pipeline::extraction::extract_text;
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

/// Heartbeat period for renewing the lease on the batch in flight. Must be
/// well under the 10-minute lease (`OUTBOX_LEASE_MINUTES` in veda-store):
/// at 3 minutes a worker survives two missed renewals before its rows
/// become claimable again as expired.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(180);

pub struct Worker {
    meta: Arc<dyn MetadataStore>,
    task_queue: Arc<dyn TaskQueue>,
    vector: Arc<dyn VectorStore>,
    memory_store: Arc<dyn MemoryStore>,
    memory_vector: Arc<dyn MemoryVectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    llm: Option<Arc<dyn LlmService>>,
    batch_size: usize,
    poll_interval: Duration,
    max_overview_tokens: usize,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        task_queue: Arc<dyn TaskQueue>,
        vector: Arc<dyn VectorStore>,
        memory_store: Arc<dyn MemoryStore>,
        memory_vector: Arc<dyn MemoryVectorStore>,
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
            memory_store,
            memory_vector,
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
            if *shutdown.borrow() {
                break;
            }
            match self.task_queue.claim(self.batch_size).await {
                Ok(tasks) if tasks.is_empty() => {
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        _ = sleep(self.poll_interval) => {}
                    }
                }
                // Deliberately NOT raced against the shutdown signal: a
                // claimed batch runs to completion so in-flight tasks get a
                // proper complete()/fail() instead of being cancelled at an
                // await point with their lease dangling. Shutdown is observed
                // at the top of the loop, before the next claim.
                Ok(tasks) => self.process_batch(tasks).await,
                Err(e) => {
                    warn!(err = %e, "claim failed");
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        _ = sleep(self.poll_interval) => {}
                    }
                }
            }
        }
        info!("worker shutting down");
    }

    /// Run one claimed batch to completion, heartbeating the lease on every
    /// task in it while any are still in flight. Already-finished tasks make
    /// the renew a no-op (it is fenced on `processing`), so no per-task
    /// bookkeeping is needed. If this process dies mid-batch the heartbeat
    /// stops with it and the leases expire — a later claim retries the rows.
    async fn process_batch(&self, tasks: Vec<OutboxEvent>) {
        let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
        let renewer = tokio::spawn({
            let queue = Arc::clone(&self.task_queue);
            async move {
                loop {
                    sleep(LEASE_RENEW_INTERVAL).await;
                    if let Err(e) = queue.renew(&ids).await {
                        warn!(err = %e, "outbox lease renew failed");
                    }
                }
            }
        });
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
        renewer.abort();
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
                // ChunkSync no longer enqueues SummarySync — the service
                // layer (`FsService::write_file`) inserts both outbox
                // events at write time, so SummarySync runs independently
                // of ChunkSync. Embedding failures (e.g. content over
                // the embedding API's per-input token cap) no longer
                // block L0/L1 generation, and the parent dir aggregation
                // no longer misses files whose chunk_sync died on a 400.
                //
                // When [llm] is not configured we still need to wipe any
                // pre-existing summary so an updated file doesn't serve
                // stale L0/L1 from a previous LLM-enabled era.
                if self.llm.is_none() {
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
            OutboxEventType::ExtractSync => {
                self.handle_extract_sync(&task.workspace_id, file_id, force_reembed)
                    .await?;
                self.task_queue.complete(task.id).await?;
            }
            OutboxEventType::MemorySync => {
                self.handle_memory_sync(&task.payload).await?;
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
            StorageType::Inline => self
                .meta
                .get_file_content(&file.id)
                .await?
                .ok_or_else(|| VedaError::NotFound(format!("content for {}", file.id))),
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
            // A blob's text lives in `veda_file_extracts`, not in the file
            // rows — pdf/word are stored byte-for-byte and only their
            // extracted layer is indexable. Reading it here is what lets
            // summary_sync summarize them; before this the worker errored
            // while `cat` (fs.rs, same freshness rule) happily served text.
            // Image/jar and stale-extract blobs still error, which is what
            // the callers' skip guards expect.
            StorageType::Blob => {
                match fresh_extract(
                    self.meta.get_file_extract(&file.id).await?,
                    &file.checksum_sha256,
                ) {
                    Some(ex) => Ok(ex.content),
                    None => Err(VedaError::InvalidInput(
                        "binary blob has no text content".into(),
                    )),
                }
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

        // A queued ChunkSync can race a rewrite that turned the file into a
        // binary blob (or be a stale / misrouted event). Blobs aren't text-
        // indexed here — drop this as a no-op instead of Err-ing into the dead
        // letters via load_full_content. Vector cleanup for type changes is
        // owned by the ChunkDelete enqueued at rewrite time; PDF vectors are
        // owned by ExtractSync, so we must NOT delete here.
        if file.storage_type == StorageType::Blob {
            debug!(file_id, "chunk_sync skipped: file is a binary blob");
            return Ok(());
        }

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

        // Binary smuggled in as valid UTF-8 (NUL-flooded image attachments
        // from folder-sync tools) is garbage to embed, and tokenizers price
        // control bytes at ~1 token/char — windows of it blow the upstream
        // per-input token cap, retrying the task into the dead letters.
        // Same semantics as empty content: nothing to index, watermark
        // advances so the file doesn't re-enter the queue.
        if is_binary_content(&content) {
            warn!(
                workspace_id,
                file_id, "chunk_sync skipped: binary content (NUL bytes), not embeddable"
            );
            self.vector.delete_chunks(workspace_id, file_id).await?;
            self.meta
                .update_file_content_hash(file_id, &file.checksum_sha256)
                .await?;
            return Ok(());
        }

        let sem_chunks = semantic_chunk(&content, 2048);
        // Drop the assembled file buffer before the embed loop allocates per-batch
        // text clones. semantic_chunk has already copied the bytes it needs.
        drop(content);
        self.embed_and_watermark(workspace_id, file_id, &file.checksum_sha256, sem_chunks)
            .await
    }

    /// Embed semantic chunks in batches, upsert to Milvus, sweep stale tail
    /// chunks, then advance the content-hash watermark. Shared by chunk_sync
    /// (text files) and extract_sync (pdf text layer).
    ///
    /// Empty input = "nothing to index": clear Milvus chunks and still
    /// watermark so the same content doesn't re-enter the queue.
    async fn embed_and_watermark(
        &self,
        workspace_id: &str,
        file_id: &str,
        checksum: &str,
        sem_chunks: Vec<SemanticChunk>,
    ) -> veda_types::Result<()> {
        if sem_chunks.is_empty() {
            self.vector.delete_chunks(workspace_id, file_id).await?;
            self.meta.update_file_content_hash(file_id, checksum).await?;
            return Ok(());
        }

        // Embed + upsert in batches so memory peaks at one batch's worth of
        // texts/embeddings/ChunkWithEmbedding rather than the entire file.
        // Upserts are idempotent (vector id keyed on file_id+chunk_index), so
        // a mid-loop failure is safe — the next outbox claim retries from the
        // first batch. The watermark below only fires after every batch lands.
        //
        // Critical: `upsert_chunks_only` per batch, then ONE
        // `delete_chunks_above` after every batch succeeds. Sweeping inside
        // each batch would delete chunk_index > batch_max after batch 1,
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
            .expect("sem_chunks non-empty checked above");
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
            .update_file_content_hash(file_id, checksum)
            .await?;
        Ok(())
    }

    /// Extract a binary blob's text layer (pdf) and embed it into Milvus for
    /// search. The original blob stays in storage untouched — only its
    /// extracted text is indexed. Extraction runs on the blocking pool;
    /// failures and panics (malformed pdf) are treated as "nothing to index"
    /// (skip + watermark), never dead-lettering, so the blob stays
    /// downloadable regardless.
    async fn handle_extract_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
        force_reembed: bool,
    ) -> veda_types::Result<()> {
        let Some(file) = self.meta.get_file(file_id).await? else {
            warn!(workspace_id, file_id, "extract_sync skipped: file no longer exists");
            self.vector.delete_chunks(workspace_id, file_id).await?;
            return Ok(());
        };

        // "Fresh" = the stored extract matches the current blob. Embeddings
        // and the extract row are maintained together, but the row can lag
        // (pre-extract-table files, a crash between upsert and watermark), so
        // it is checked independently: a watermark hit with a stale/missing
        // extract still re-extracts below — it only skips the embed.
        let extract_fresh = fresh_extract(
            self.meta.get_file_extract(file_id).await?,
            &file.checksum_sha256,
        )
        .is_some();
        let embed_current = !force_reembed
            && file.last_embedded_content_hash.as_deref() == Some(file.checksum_sha256.as_str());
        if embed_current && extract_fresh {
            debug!(file_id, "extract_sync skipped: content unchanged since last embed");
            return Ok(());
        }

        let Some(data) = self.meta.get_file_blob(file_id).await? else {
            warn!(workspace_id, file_id, "extract_sync skipped: blob missing");
            return Ok(());
        };

        // PDF/Word parsing is CPU-heavy and can panic on malformed input. Run
        // it on the blocking pool; a panic surfaces as a JoinError and is
        // handled as a skip rather than crashing the worker or dead-lettering
        // the file.
        let mime = file.mime_type.clone();
        let extracted = tokio::task::spawn_blocking(move || extract_text(&data, &mime)).await;
        let text = match extracted {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                warn!(workspace_id, file_id, error = %e, "extract_sync skipped: extraction failed");
                // Drop any vectors and stored text from a prior successful
                // extract so a blob rewritten into an unextractable one stops
                // serving stale content, then watermark so we don't retry the
                // doomed extract forever.
                self.vector.delete_chunks(workspace_id, file_id).await?;
                self.meta.delete_file_extract(file_id).await?;
                self.meta
                    .update_file_content_hash(file_id, &file.checksum_sha256)
                    .await?;
                return Ok(());
            }
            Err(join_err) => {
                warn!(workspace_id, file_id, error = %join_err, "extract_sync skipped: extractor panicked");
                self.vector.delete_chunks(workspace_id, file_id).await?;
                self.meta.delete_file_extract(file_id).await?;
                self.meta
                    .update_file_content_hash(file_id, &file.checksum_sha256)
                    .await?;
                return Ok(());
            }
        };

        // Persist the full text before embedding: read paths can serve it
        // immediately, and a crash mid-embed re-runs this upsert (idempotent,
        // keyed on file_id) on the next outbox claim.
        let extract = veda_types::FileExtract {
            file_id: file_id.to_string(),
            content: text,
            source_sha256: file.checksum_sha256.clone(),
        };
        self.meta.upsert_file_extract(&extract).await?;

        // Pdf/Word acquire a text layer only here, so this is where their
        // SummarySync is born — `enqueue_index_outbox` cannot do it at write
        // time (no extract exists yet, and a SummarySync claimed before this
        // handler would find nothing to summarize). Without this hop
        // pdf/word files never got an L0/L1 at all, and their parent
        // directories aggregated without them.
        //
        // Placement is load-bearing: after the upsert, so the task is
        // guaranteed to find fresh text, and *before* the `embed_current`
        // early return below so the refresh-only path enqueues too.
        // enqueue_dedup collapses repeats against a pending row, so a
        // re-extracted file does not queue a second summary.
        let inserted = enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            OutboxEventType::SummarySync,
            "file_id",
            file_id,
            serde_json::json!({ "file_id": file_id }),
            Utc::now(),
        )
        .await?;
        if !inserted {
            info!(file_id, "summary_sync already pending, skipping enqueue");
        }

        if embed_current {
            debug!(file_id, "extract_sync: refreshed stored extract; embeddings already current");
            return Ok(());
        }

        let sem_chunks = semantic_chunk(&extract.content, 2048);
        drop(extract);
        self.embed_and_watermark(workspace_id, file_id, &file.checksum_sha256, sem_chunks)
            .await
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

        // Blobs are summarizable only through their extracted text layer
        // (pdf/word). Two kinds reach here without one and must be skipped,
        // not errored: true binaries (images, jars) that never get extracted,
        // and stale SummarySyncs pointing at a file that was text when the
        // task was enqueued and binary by the time the worker ran. Erroring
        // sent 315 such orphans to dead-letter in prod (2026-07-13) — skip
        // instead; the blob stays downloadable either way.
        //
        // Freshness is not optional: a stale extract is the *previous*
        // revision's text, so summarizing it would publish an L0/L1 that
        // describes content the file no longer has.
        if file.storage_type == StorageType::Blob
            && fresh_extract(
                self.meta.get_file_extract(file_id).await?,
                &file.checksum_sha256,
            )
            .is_none()
        {
            warn!(workspace_id, file_id, "summary_sync skipped: blob has no extracted text");
            return Ok(());
        }

        let content = self.load_full_content(&file).await?;

        if content.trim().is_empty() {
            return Ok(());
        }

        let max_tokens = self.max_overview_tokens;
        let (l0, l1) = tokio::try_join!(
            summary::generate_l0(llm.as_ref(), &content, max_tokens),
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
        let child_summaries = self
            .meta
            .list_child_summaries(workspace_id, dir_path)
            .await?;
        if child_summaries.is_empty() {
            // Directory has no children with summaries (just got emptied
            // or never had any). Delete the previously-aggregated
            // summary row so the dir doesn't keep serving a stale L0/L1
            // describing files that were deleted. Best-effort: a missing
            // row is a no-op, so no need to check existence first.
            self.meta.delete_summary_by_dentry(dentry_id).await?;
            // Vector store keys dir summaries by dentry_id (see the
            // `upsert_summaries` call below in the non-empty branch),
            // so the same id deletes it.
            self.vector
                .delete_summary(workspace_id, dentry_id)
                .await?;
            // Cascade to the parent — its aggregate may also need to drop
            // this dir's L0 if it just turned empty.
            let parent = parent_path_of(dir_path);
            if parent != dir_path {
                if let Some(parent_dentry) =
                    self.meta.get_dentry(workspace_id, &parent).await?
                {
                    self.enqueue_dir_summary_sync(
                        workspace_id,
                        &parent_dentry.id,
                        &parent,
                    )
                    .await?;
                }
            }
            return Ok(());
        }

        // Aggregation needs an LLM. Cleanup above does not, so it runs
        // regardless — keeps the dir summary fresh on delete/rename
        // even when no LLM is configured.
        let Some(llm) = &self.llm else {
            return Ok(());
        };

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

    /// Heal the memory vector index after a synchronous write failed
    /// (docs/plans/agent-memory-m1.md §2). Deletes are a plain retry;
    /// upserts re-read the row under its own scope (payload carries it —
    /// even infra reads go through the scope-filtered primitive), re-embed,
    /// and re-upsert. A row that vanished or expired in the meantime is
    /// simply done: the recheck read path already guarantees correctness,
    /// this task only restores recall.
    async fn handle_memory_sync(&self, payload: &serde_json::Value) -> Result<()> {
        let memory_id = payload
            .get("memory_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| VedaError::Internal("memory_sync payload missing memory_id".into()))?;
        let op = payload.get("op").and_then(|v| v.as_str()).unwrap_or("upsert");
        if op == "delete" {
            return self.memory_vector.delete_memory_vectors(&[memory_id]).await;
        }
        let scope_type = payload
            .get("scope_type")
            .and_then(|v| v.as_str())
            .and_then(MemoryScopeType::parse)
            .ok_or_else(|| VedaError::Internal("memory_sync payload missing scope_type".into()))?;
        let scope_id = payload
            .get("scope_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VedaError::Internal("memory_sync payload missing scope_id".into()))?;
        let filter = MemoryScopeFilter::Scope {
            scope_type,
            scope_id: scope_id.to_string(),
        };
        let rows = self
            .memory_store
            .get_memories_by_ids(&[memory_id], &filter)
            .await?;
        let Some(memory) = rows.into_iter().next() else {
            debug!(memory_id, "memory gone before index heal, nothing to do");
            return Ok(());
        };
        let mut vectors = self.embedding.embed(&[memory.content.clone()]).await?;
        let vector = vectors
            .pop()
            .ok_or_else(|| VedaError::EmbeddingFailed("empty embedding batch".into()))?;
        self.memory_vector
            .upsert_memory_vectors(&[MemoryWithEmbedding {
                id: memory.id,
                scope_type: memory.scope_type,
                scope_id: memory.scope_id.clone(),
                origin_workspace_id: memory.origin_workspace_id.clone().unwrap_or_default(),
                content: memory.content.clone(),
                vector,
            }])
            .await
    }
}

fn parent_path_of(path: &str) -> String {
    veda_core::path::parent(path).to_string()
}

/// The stored text layer of a blob, but only when it still describes the
/// blob's current bytes. `source_sha256` is stamped at extraction time, so a
/// row left over from a previous revision must be treated as *absent*: it
/// would otherwise be embedded, summarized, or served as if it were the
/// current content.
///
/// Every blob text-layer decision in this worker funnels through here —
/// extract_sync's "already fresh" check, summary_sync's blob gate, and
/// load_full_content — so the three cannot drift into disagreeing about what
/// "has extracted text" means. Mirrors the read path in
/// `veda-core/service/fs.rs`, which applies the same comparison before
/// serving extracted text to `cat`.
fn fresh_extract(extract: Option<FileExtract>, checksum: &str) -> Option<FileExtract> {
    extract.filter(|ex| ex.source_sha256 == checksum)
}

/// Static label string for `OutboxEventType`, used as a Prometheus label.
/// Must be `&'static str` because `metrics` labels need it.
fn outbox_event_label(event: OutboxEventType) -> &'static str {
    match event {
        OutboxEventType::ChunkSync => "chunk_sync",
        OutboxEventType::ChunkDelete => "chunk_delete",
        OutboxEventType::SummarySync => "summary_sync",
        OutboxEventType::DirSummarySync => "dir_summary_sync",
        OutboxEventType::ExtractSync => "extract_sync",
        OutboxEventType::MemorySync => "memory_sync",
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

    // ── blob text-layer freshness ──────────────────────────────────────
    //
    // `fresh_extract` is the whole decision behind two guards that a mocked
    // store cannot reach cheaply (MetadataStore has ~100 methods and this
    // crate has no fake for it): summary_sync's blob gate skips exactly when
    // this returns None, and load_full_content errors exactly when it returns
    // None. The tables below are those guards' truth tables; that the guards
    // are wired to them is covered by
    // `tests/blob_summary_test.rs` against real MySQL.

    const CURRENT: &str = "sha-current";
    const PREVIOUS: &str = "sha-previous";

    fn extract_row(sha: &str) -> FileExtract {
        FileExtract {
            file_id: "f1".into(),
            content: "extracted body".into(),
            source_sha256: sha.into(),
        }
    }

    /// Gate 3 state 1/3 — pdf/word: extract matches the blob's current bytes,
    /// so summary_sync must NOT skip. This is the case the fix exists for.
    #[test]
    fn gate3_blob_with_fresh_extract_is_summarizable() {
        assert!(fresh_extract(Some(extract_row(CURRENT)), CURRENT).is_some());
    }

    /// Gate 3 state 2/3 — blob rewritten since extraction. Summarizing the
    /// leftover row would describe the *previous* revision, so it must skip
    /// and wait for extract_sync to refresh the text.
    #[test]
    fn gate3_blob_with_stale_extract_is_skipped() {
        assert!(fresh_extract(Some(extract_row(PREVIOUS)), CURRENT).is_none());
    }

    /// Gate 3 state 3/3 — image / jar: never extracted, nothing to summarize.
    /// Preserves the 2026-07-13 hardening (skip, don't error) that cleared
    /// 315 dead letters in prod.
    #[test]
    fn gate3_blob_without_extract_is_skipped() {
        assert!(fresh_extract(None, CURRENT).is_none());
    }

    /// Gate 4 state 1/2 — load_full_content serves the extracted text for a
    /// fresh blob rather than erroring, matching what `cat` already returns.
    #[test]
    fn gate4_fresh_extract_yields_content() {
        let got = fresh_extract(Some(extract_row(CURRENT)), CURRENT).map(|ex| ex.content);
        assert_eq!(got.as_deref(), Some("extracted body"));
    }

    /// Gate 4 state 2/2 — a stale row yields nothing, so the caller raises
    /// "binary blob has no text content" instead of embedding old text.
    #[test]
    fn gate4_stale_extract_yields_nothing() {
        let got = fresh_extract(Some(extract_row(PREVIOUS)), CURRENT).map(|ex| ex.content);
        assert_eq!(got, None);
    }
}

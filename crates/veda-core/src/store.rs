use async_trait::async_trait;
use veda_types::*;

/// Batch path-resolution result: the dentry a file_id maps to. When one
/// file_id has several dentries (`copy_file` aliases), implementations MUST
/// pick deterministically (smallest `path`) so access-stat attribution
/// doesn't drift between queries.
#[derive(Debug, Clone)]
pub struct DentryPathRef {
    pub dentry_id: String,
    pub path: String,
}

/// One (workspace, day, dentry) delta flushed by the access recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocAccessRow {
    pub workspace_id: String,
    pub day: chrono::NaiveDate,
    pub dentry_id: String,
    pub search_hits: u64,
    pub reads: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocAccessOrder {
    Reads,
    SearchHits,
}

// ── Metadata Store ─────────────────────────────────────

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn ping(&self) -> Result<()>;

    /// Per-workspace backlog of index-affecting outbox tasks — only
    /// `chunk_sync` + `extract_sync` (the events that gate searchability;
    /// summary events debounce for 30s and would read as a permanently
    /// non-zero backlog). Returns `(pending, processing, dead)` counts.
    /// Serves `GET /v1/index-status`. Default impl errs so test mocks
    /// that never touch indexing don't have to stub it.
    async fn count_index_backlog(&self, workspace_id: &str) -> Result<(i64, i64, i64)> {
        let _ = workspace_id;
        Err(VedaError::Internal(
            "count_index_backlog not implemented by this store".into(),
        ))
    }

    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn list_dentries(&self, workspace_id: &str, parent_path: &str) -> Result<Vec<Dentry>>;
    /// Direct children of `parent_path`, directories first and each group
    /// ordered by `path`, capped at `limit`.
    ///
    /// `list_dentries` is an unbounded `fetch_all`; the workspace map only
    /// renders ~200 entries but would otherwise load every root-level row
    /// (and build an `IN (...)` over every root-level file id) just to throw
    /// them away. Ordering is pushed into SQL so the cap keeps the entries
    /// that survive truncation — directories are subtrees worth naming,
    /// loose root files are not.
    async fn list_children_capped(
        &self,
        workspace_id: &str,
        parent_path: &str,
        limit: usize,
    ) -> Result<Vec<Dentry>>;
    /// File counts grouped by top-level path segment (`/docs/a/b.md` ->
    /// `docs`). Lets the map answer "how big is this area" without walking
    /// each subtree.
    ///
    /// Cost is O(dentries in the workspace): the GROUP BY is on an
    /// expression, so no index can serve the grouping — only the leading
    /// `workspace_id` of `idx_ws_path` narrows the scan. Same order of
    /// magnitude as `storage_stats`, which the map also calls.
    async fn count_files_by_top_level(
        &self,
        workspace_id: &str,
    ) -> Result<std::collections::HashMap<String, i64>>;
    /// Return up to `limit` dentries under `path_prefix` ordered by `path`
    /// ASC, strictly after `after_path` (exclusive cursor; `None` starts
    /// from the beginning). Caller pages by passing the last returned
    /// `path` as the next `after_path`, stopping when fewer than `limit`
    /// rows come back.
    ///
    /// Stable sort by `path` is REQUIRED so paging is deterministic
    /// across invocations even with concurrent writes.
    async fn list_dentries_under_page(
        &self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>>;

    /// Convenience wrapper that drains `list_dentries_under_page` until
    /// exhausted, capped at `max` total entries. Errors with
    /// `QuotaExceeded` if the listing would exceed `max` — protects
    /// callers from unbounded memory growth on large workspaces.
    /// For genuinely unbounded scans (reconciler), call `_page` directly
    /// in a streaming loop and process incrementally.
    async fn list_dentries_under_capped(
        &self,
        workspace_id: &str,
        path_prefix: &str,
        max: usize,
    ) -> Result<Vec<Dentry>> {
        const PAGE_SIZE: usize = 1000;
        let mut out: Vec<Dentry> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .list_dentries_under_page(
                    workspace_id,
                    path_prefix,
                    cursor.as_deref(),
                    PAGE_SIZE,
                )
                .await?;
            let n = page.len();
            if out.len() + n > max {
                return Err(VedaError::QuotaExceeded(format!(
                    "dentry scan under {path_prefix} exceeded {max} entries"
                )));
            }
            if let Some(last) = page.last() {
                cursor = Some(last.path.clone());
            }
            out.extend(page);
            if n < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }
    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>>;
    async fn get_files_batch(&self, file_ids: &[String]) -> Result<Vec<FileRecord>> {
        let mut results = Vec::with_capacity(file_ids.len());
        for id in file_ids {
            if let Some(f) = self.get_file(id).await? {
                results.push(f);
            }
        }
        Ok(results)
    }
    async fn get_file_content(&self, file_id: &str) -> Result<Option<String>>;
    /// Read raw blob bytes for a binary file (storage_type = Blob).
    async fn get_file_blob(&self, file_id: &str) -> Result<Option<Vec<u8>>>;
    /// Read the stored extracted text of an extractable blob (pdf/word).
    async fn get_file_extract(&self, file_id: &str) -> Result<Option<FileExtract>>;
    /// Insert-or-replace the extracted text for a blob. Written by the
    /// ExtractSync worker after a successful extraction.
    async fn upsert_file_extract(&self, extract: &FileExtract) -> Result<()>;
    /// Drop the extracted text row (extraction failed / blob unextractable).
    async fn delete_file_extract(&self, file_id: &str) -> Result<()>;
    async fn get_file_chunks(
        &self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>>;
    /// Lightweight metadata read of `(chunk_index, byte_len)` for every chunk
    /// of a file, ordered by `chunk_index`. Excludes the `content` column so
    /// callers can compute byte offsets cheaply (used by byte-range reads to
    /// figure out which chunks actually overlap the requested range).
    /// Default impl falls back to `get_file_chunks` — fine for in-memory
    /// mocks, real stores should override.
    async fn list_chunk_byte_lens(&self, file_id: &str) -> Result<Vec<(i32, i32)>> {
        let chunks = self.get_file_chunks(file_id, None, None).await?;
        Ok(chunks
            .into_iter()
            .map(|c| (c.chunk_index, c.byte_len))
            .collect())
    }
    /// Fetch chunks by a closed `chunk_index` range `[idx_min, idx_max]`,
    /// ordered by `chunk_index`. Used by byte-range reads after computing
    /// the overlapping chunk indices via `list_chunk_byte_lens`.
    /// Default impl falls back to `get_file_chunks` + filter — correct but
    /// pulls full content for mocks. Real stores should override.
    async fn get_chunks_in_index_range(
        &self,
        file_id: &str,
        idx_min: i32,
        idx_max: i32,
    ) -> Result<Vec<FileChunk>> {
        let chunks = self.get_file_chunks(file_id, None, None).await?;
        Ok(chunks
            .into_iter()
            .filter(|c| c.chunk_index >= idx_min && c.chunk_index <= idx_max)
            .collect())
    }
    async fn find_file_by_checksum(
        &self,
        workspace_id: &str,
        checksum: &str,
    ) -> Result<Option<FileRecord>>;
    /// Idempotent directory insert: succeeds silently if the dentry already
    /// exists. Used by `ensure_parents` outside a transaction so that parent
    /// directory creation does not hold row locks.
    async fn insert_dentry_ignore(&self, dentry: &Dentry) -> Result<()> {
        let mut tx = self.begin_tx().await?;
        match tx.insert_dentry(dentry).await {
            Ok(()) => tx.commit().await,
            Err(VedaError::AlreadyExists(_)) => {
                tx.rollback().await.ok();
                Ok(())
            }
            Err(e) => {
                tx.rollback().await.ok();
                Err(e)
            }
        }
    }
    async fn get_dentry_path_by_file_id(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<String>>;
    /// Batch file_id → dentry resolution for search-hit display and access
    /// counting. No default implementation: a per-id loop would be N+1 and
    /// couldn't honor the deterministic-alias contract of [`DentryPathRef`].
    async fn get_dentry_paths_by_file_ids(
        &self,
        workspace_id: &str,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, DentryPathRef>>;
    async fn query_fs_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FsEvent>>;
    /// Smallest still-retained `veda_fs_events.id` for a workspace, or
    /// `None` if the workspace has no events yet. Returning `Some(min)`
    /// where `since_id < min` is the cue to emit HTTP 410 to a stale SSE
    /// client — they slept past the retention window and need to resync.
    async fn min_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>>;
    /// Largest `veda_fs_events.id` for a workspace, or `None` if empty.
    /// Returned alongside `current_min_id` in the 410 body so the client
    /// has a race-free resync cursor: list_dir + resubscribe with
    /// `since_id = current_max_id` won't miss events landed mid-recovery.
    async fn max_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>>;
    /// Delete `veda_fs_events` rows with `created_at < cutoff` across all
    /// workspaces. Returns the number of rows removed. Called periodically
    /// by the retention task.
    async fn prune_fs_events_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64>;
    /// Insert a single `FsEvent` row outside of a transaction. Convenience
    /// for callers that don't need to bundle event emission into a larger
    /// metadata change (e.g. the summary worker emitting `summary_ready`
    /// after a self-contained `upsert_summary`). Transactional inserts
    /// still go through [`MetadataTx::insert_fs_event`].
    async fn insert_fs_event_direct(&self, event: &FsEvent) -> Result<()>;
    async fn storage_stats(&self, workspace_id: &str) -> Result<StorageStats>;
    /// Update the `last_embedded_content_hash` watermark after a successful
    /// Milvus upsert. Worker uses this on the next claim to skip redundant
    /// embed calls when content hash is unchanged.
    async fn update_file_content_hash(&self, file_id: &str, hash: &str) -> Result<()>;
    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>>;

    // summary ops (L0/L1)
    async fn get_summary_by_file(&self, file_id: &str) -> Result<Option<FileSummary>>;
    async fn get_summaries_by_file_ids(
        &self,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FileSummary>> {
        let mut map = std::collections::HashMap::new();
        for fid in file_ids {
            if let Some(s) = self.get_summary_by_file(fid).await? {
                map.insert(fid.clone(), s);
            }
        }
        Ok(map)
    }
    async fn get_summary_by_dentry(&self, dentry_id: &str) -> Result<Option<FileSummary>>;
    /// Batch sibling of `get_summaries_by_file_ids`, keyed by dentry_id —
    /// directory summaries hang off a dentry, not a file. Deliberately has
    /// no default implementation: a looping default would silently be N+1.
    async fn get_summaries_by_dentry_ids(
        &self,
        dentry_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FileSummary>>;
    /// Return (file_id_set, dentry_id_set) of every `Ready` summary in
    /// `workspace_id`. Reconciler bulk-checks the dentry list against
    /// these sets, replacing per-dentry get_summary_by_* lookups (O(N)
    /// round-trips → 1). Implementations MUST execute a single query.
    async fn list_ready_summary_keys(
        &self,
        workspace_id: &str,
    ) -> Result<(
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    )>;
    // doc access stats (heat counters)
    /// Apply per-day access deltas. All rows MUST land in ONE transaction:
    /// with partial application the caller can't retry without
    /// double-counting the batches that already committed (review 2026-08-05).
    async fn upsert_doc_access_daily(&self, rows: &[DocAccessRow]) -> Result<()>;
    /// Heat ranking joined against live dentries (deleted docs drop out,
    /// renamed docs keep their history via dentry_id). `since` is inclusive.
    async fn query_doc_access(
        &self,
        workspace_id: &str,
        since: chrono::NaiveDate,
        order: DocAccessOrder,
        limit: usize,
    ) -> Result<Vec<api::DocAccessEntry>>;
    /// Chunked delete of stats rows with `day < cutoff`. Returns rows removed.
    async fn sweep_doc_access(&self, cutoff: chrono::NaiveDate) -> Result<u64>;

    async fn upsert_summary(&self, summary: &FileSummary) -> Result<()>;
    async fn delete_summary_by_file(&self, file_id: &str) -> Result<()>;
    /// Delete the directory summary row keyed by dentry_id. Used by the
    /// worker when a directory aggregation finds no children (the dir
    /// is now empty), so future reads return "no summary" instead of
    /// the pre-empty stale aggregate.
    async fn delete_summary_by_dentry(&self, dentry_id: &str) -> Result<()>;
    async fn list_child_summaries(
        &self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<Vec<FileSummary>>;
}

#[async_trait]
pub trait MetadataTx: Send {
    // dentry ops
    async fn get_dentry(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()>;
    async fn update_dentry_file_id(
        &mut self,
        workspace_id: &str,
        path: &str,
        file_id: &str,
    ) -> Result<()>;
    async fn delete_dentry(&mut self, workspace_id: &str, path: &str) -> Result<u64>;
    /// See `MetadataStore::list_dentries_under_page`. Same contract
    /// applied within an open transaction.
    async fn list_dentries_under_page(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>>;
    /// See `MetadataStore::list_dentries_under_capped`.
    async fn list_dentries_under_capped(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
        max: usize,
    ) -> Result<Vec<Dentry>> {
        const PAGE_SIZE: usize = 1000;
        let mut out: Vec<Dentry> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .list_dentries_under_page(
                    workspace_id,
                    path_prefix,
                    cursor.as_deref(),
                    PAGE_SIZE,
                )
                .await?;
            let n = page.len();
            if out.len() + n > max {
                return Err(VedaError::QuotaExceeded(format!(
                    "dentry scan under {path_prefix} exceeded {max} entries"
                )));
            }
            if let Some(last) = page.last() {
                cursor = Some(last.path.clone());
            }
            out.extend(page);
            if n < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }
    async fn delete_dentries_under(&mut self, workspace_id: &str, parent_path: &str)
        -> Result<u64>;
    async fn rename_dentry(
        &mut self,
        workspace_id: &str,
        old_path: &str,
        new_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<()>;
    /// Batch-rename all dentries under `old_prefix` to `new_prefix` in a single
    /// statement. E.g. renaming `/a` to `/b` rewrites `/a/x` → `/b/x`.
    async fn rename_dentries_under(
        &mut self,
        workspace_id: &str,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<u64>;

    // file ops
    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>>;
    async fn insert_file(&mut self, file: &FileRecord) -> Result<()>;
    async fn update_file_revision(
        &mut self,
        file_id: &str,
        expected_rev: i32,
        new_rev: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
        mime_type: &str,
        source_type: SourceType,
    ) -> Result<()>;
    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32>;
    async fn increment_ref_count(&mut self, file_id: &str) -> Result<()>;
    async fn delete_file(&mut self, file_id: &str) -> Result<()>;

    // content ops (read + write)
    async fn get_file_content(&mut self, file_id: &str) -> Result<Option<String>>;
    async fn get_file_chunks(
        &mut self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>>;
    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()>;
    async fn delete_file_content(&mut self, file_id: &str) -> Result<()>;
    async fn insert_file_blob(&mut self, file_id: &str, data: &[u8]) -> Result<()>;
    async fn delete_file_blob(&mut self, file_id: &str) -> Result<()>;
    /// Drop the extracted-text row alongside the blob it came from. Called in
    /// the same transaction as content/chunks/blob deletion on every rewrite
    /// and delete path so a stale extract can never outlive its blob.
    async fn delete_file_extract(&mut self, file_id: &str) -> Result<()>;
    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()>;
    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()>;
    /// Delete chunks with `chunk_index >= from_chunk_index`. Used by incremental
    /// append to prune the trailing chunk(s) before re-inserting rebalanced ones.
    async fn delete_file_chunks_from(&mut self, file_id: &str, from_chunk_index: i32)
        -> Result<()>;

    // outbox
    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()>;
    /// Insert an outbox event only if no pending/processing event of the same
    /// `event_type` already exists for this `(workspace_id, file_id)`. Returns
    /// `true` if inserted, `false` if a duplicate was detected and the call
    /// was a no-op. Use for ChunkSync/ChunkDelete to deduplicate rapid writes
    /// to the same file.
    async fn try_insert_outbox_for_file(
        &mut self,
        event: &OutboxEvent,
        file_id: &str,
    ) -> Result<bool>;

    // fs event
    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()>;
    async fn insert_fs_events(&mut self, events: &[FsEvent]) -> Result<()> {
        for e in events {
            self.insert_fs_event(e).await?;
        }
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()>;
    async fn rollback(self: Box<Self>) -> Result<()>;
}

// ── Vector Store ───────────────────────────────────────

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn ping(&self) -> Result<()>;
    async fn upsert_chunks(&self, chunks: &[ChunkWithEmbedding]) -> Result<()>;
    /// Same as `upsert_chunks` but skips the trailing "delete chunk_index >
    /// max_in_batch" sweep. Use when streaming a single file's chunks in
    /// multiple batches — call this for every batch, then `delete_chunks_above`
    /// once at the end with the overall max chunk_index. Default impl falls
    /// back to `upsert_chunks` (which does the sweep itself); only override
    /// for stores where the sweep is not safe between partial batches.
    async fn upsert_chunks_only(&self, chunks: &[ChunkWithEmbedding]) -> Result<()> {
        self.upsert_chunks(chunks).await
    }
    /// Delete chunks with `chunk_index > max_chunk_index` for `(workspace_id,
    /// file_id)`. Pair with `upsert_chunks_only` to avoid the
    /// "first batch upserts, second batch fails, search index missing the
    /// tail" hole that batched embed otherwise opens up. Default impl noops
    /// (fine for in-memory test stores).
    async fn delete_chunks_above(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _max_chunk_index: i32,
    ) -> Result<()> {
        Ok(())
    }
    async fn delete_chunks(&self, workspace_id: &str, file_id: &str) -> Result<()>;
    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;

    async fn upsert_summaries(&self, summaries: &[SummaryWithEmbedding]) -> Result<()>;
    async fn delete_summary(&self, workspace_id: &str, id: &str) -> Result<()>;
    async fn search_summaries(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;

    /// Return the distinct `file_id` set present in the chunks collection
    /// for `workspace_id`. Used by the reconciler to diff against MySQL.
    /// Implementations may paginate internally; the returned list is a
    /// point-in-time snapshot (no consistency guarantees during enumeration).
    async fn list_chunk_file_ids(&self, workspace_id: &str) -> Result<Vec<String>>;
    /// Return the distinct entity IDs present in the summary collection for
    /// `workspace_id`. Summary entity IDs are file_id (for file summaries)
    /// or dentry_id (for directory summaries) — reconciler resolves both.
    async fn list_summary_ids(&self, workspace_id: &str) -> Result<Vec<String>>;

    /// Initialise vector store collections and indexes. No-op if collections
    /// already exist. Demo-stage: schema mismatches are not auto-migrated;
    /// drop the collection by hand if you need a fresh schema.
    async fn init_collections(&self, embedding_dim: u32) -> Result<()>;
}

// ── Task Queue ─────────────────────────────────────────

#[async_trait]
pub trait TaskQueue: Send + Sync {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()>;
    /// Claim up to `batch_size` runnable tasks, marking them `processing`
    /// under a fresh lease. Rows whose lease expired (executor crashed and
    /// stopped heartbeating) are re-claimable; the re-claim counts as a
    /// retry. Single-worker deployment: there is no per-owner fencing —
    /// lifecycle calls fence on `status = 'processing'` alone, and the
    /// content-hash watermark makes a rare duplicate execution idempotent.
    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>>;
    /// Mark a task completed. No-op if the row is no longer `processing`
    /// (duplicate completion, or the row was re-driven to a terminal state
    /// after its lease expired).
    async fn complete(&self, task_id: i64) -> Result<()>;
    /// Record a failure: re-pend with backoff, or dead-letter once retries
    /// are exhausted. Same `processing`-status fencing as `complete`.
    async fn fail(&self, task_id: i64, error: &str) -> Result<()>;
    /// Heartbeat: extend the lease on the given tasks while they are still
    /// `processing`. Finished rows are silently skipped, so callers can
    /// renew a whole claimed batch without tracking per-task completion.
    async fn renew(&self, task_ids: &[i64]) -> Result<()>;
    async fn has_pending_event(
        &self,
        event_type: OutboxEventType,
        workspace_id: &str,
        payload_key: &str,
        payload_value: &str,
    ) -> Result<bool>;
    /// Delete `veda_outbox` rows in a terminal status (`completed` / `dead`)
    /// whose `created_at < cutoff`, across all workspaces. Returns the number
    /// of rows removed. Pending and processing rows are never touched. Called
    /// periodically by the outbox retention task in `veda-server`.
    async fn prune_outbox_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64>;
}

// ── Collection Meta Store ──────────────────────────────

#[async_trait]
pub trait CollectionMetaStore: Send + Sync {
    async fn create_collection_schema(&self, schema: &CollectionSchema) -> Result<()>;
    async fn get_collection_schema(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<CollectionSchema>>;
    async fn get_collection_schema_by_id(&self, id: &str) -> Result<Option<CollectionSchema>>;
    async fn list_collection_schemas(&self, workspace_id: &str) -> Result<Vec<CollectionSchema>>;
    async fn delete_collection_schema(&self, id: &str) -> Result<()>;
}

// ── Collection Vector Store ────────────────────────────

#[async_trait]
pub trait CollectionVectorStore: Send + Sync {
    async fn create_dynamic_collection(
        &self,
        name: &str,
        fields: &[FieldDefinition],
        embedding_dim: u32,
    ) -> Result<()>;
    async fn drop_dynamic_collection(&self, name: &str) -> Result<()>;
    async fn insert_collection_rows(
        &self,
        collection_name: &str,
        workspace_id: &str,
        rows: &[serde_json::Value],
    ) -> Result<()>;
    async fn search_collection(
        &self,
        collection_name: &str,
        workspace_id: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
    async fn query_collection(
        &self,
        collection_name: &str,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
}

// ── Auth Store ─────────────────────────────────────────

#[async_trait]
pub trait AuthStore: Send + Sync {
    // account
    async fn create_account(&self, account: &Account) -> Result<()>;
    async fn get_account(&self, id: &str) -> Result<Option<Account>>;
    async fn get_account_by_email(&self, email: &str) -> Result<Option<Account>>;
    /// Look up an account by its platform `app_id` (unique when set). Used by
    /// the app_id-scoped control plane (`/v1/apps/{app_id}/...`) to resolve or
    /// auto-provision the tenant without a `vk_` token.
    async fn get_account_by_app_id(&self, app_id: &str) -> Result<Option<Account>>;
    /// Attach an email + password hash to an existing anonymous
    /// account (rows with `email IS NULL`), optionally renaming it.
    /// Used by `veda init --upgrade` to turn anon identities into recoverable
    /// ones. The implementation guards `WHERE email IS NULL`, so a
    /// concurrent claim that already won the race surfaces as
    /// `Unauthorized` (zero rows affected), and a stale email collision
    /// surfaces as `AlreadyExists` (MySQL 1062 mapped). Caller hashes
    /// the password.
    async fn claim_account(
        &self,
        id: &str,
        email: &str,
        password_hash: &str,
        name: Option<&str>,
    ) -> Result<()>;

    /// Atomically create an account + its `vk_` api key + a default
    /// workspace + a `wk_` workspace key inside one transaction. Used
    /// by anonymous onboarding so a mid-way failure can't leave an
    /// orphan account with no workspace.
    async fn create_anonymous_bundle(
        &self,
        account: &Account,
        api_key: &ApiKeyRecord,
        workspace: &Workspace,
        ws_key: &WorkspaceKey,
    ) -> Result<()>;

    // api key
    async fn create_api_key(&self, key: &ApiKeyRecord) -> Result<()>;
    async fn get_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRecord>>;
    /// Look up an api key by its public id (NOT the hash). Used by the
    /// admin disable endpoint to verify ownership before revoking — without
    /// this, any account could revoke any token by id.
    async fn get_api_key_by_id(&self, id: &str) -> Result<Option<ApiKeyRecord>>;
    async fn list_api_keys(&self, account_id: &str) -> Result<Vec<ApiKeyRecord>>;
    async fn revoke_api_key(&self, id: &str) -> Result<()>;

    // workspace
    async fn create_workspace(&self, workspace: &Workspace) -> Result<()>;

    /// Stamp creator identity onto a workspace (apps surface only): `creator`
    /// is the gateway domain account, `creator_name` the display name. Both
    /// NULL on direct (non-gateway) access. Kept separate from create so the
    /// shared create path stays unchanged.
    async fn set_workspace_creator(
        &self,
        workspace_id: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()>;

    /// Update a workspace's mutable fields (apps surface): `name` + `description`
    /// (`kind` is immutable). A name collision with another workspace under the
    /// same account surfaces as `AlreadyExists` (UNIQUE(account_id, name)).
    async fn update_workspace(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<()>;

    /// Fetch a workspace's creator identity (apps surface): `(creator,
    /// creator_name)`; `(None, None)` when unset or the workspace is absent.
    async fn get_workspace_creator(
        &self,
        id: &str,
    ) -> Result<(Option<String>, Option<String>)>;

    /// Offset-paginated list of an account's active workspaces WITH creator
    /// identity (apps surface). Returns `(page items, total count)`. `order_by`
    /// (`created_at` | `id`) and `order` (`asc` | `desc`) are whitelisted by the
    /// impl — never interpolate caller input into SQL directly.
    #[allow(clippy::type_complexity)]
    async fn list_app_workspaces(
        &self,
        account_id: &str,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Workspace, Option<String>, Option<String>)>, i64)>;

    /// Like `list_app_workspaces` but flattened across several accounts — a
    /// user's projects span every workspace they can access. Empty
    /// `account_ids` → empty page (no query).
    async fn list_app_workspaces_for_accounts(
        &self,
        account_ids: &[String],
        keyword: Option<&str>,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Workspace, Option<String>, Option<String>)>, i64)>;

    /// Create a workspace key on the apps surface, persisting the plaintext
    /// `token` (for getToken) and creator identity alongside the hash.
    async fn create_app_workspace_key(
        &self,
        key: &WorkspaceKey,
        token: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()>;

    /// List a workspace's keys WITH plaintext token + creator (apps surface),
    /// so the handler can return a masked token and the creator.
    #[allow(clippy::type_complexity)]
    async fn list_app_workspace_keys(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<(WorkspaceKey, Option<String>, Option<String>, Option<String>)>>;

    /// Fetch a key's plaintext token by id, scoped to its workspace (getToken).
    /// `None` when the key is absent, not in that workspace, or has no stored
    /// token (pre-apps keys).
    async fn get_workspace_key_token(
        &self,
        key_id: &str,
        workspace_id: &str,
    ) -> Result<Option<String>>;

    /// Stamp creator identity onto a dataset (apps surface). NULL on direct access.
    async fn set_dataset_creator(
        &self,
        dataset_id: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()>;

    /// Offset-paginated list of a workspace's active datasets WITH creator
    /// identity (apps surface). Returns `(page items, total count)`; `order_by`
    /// / `order` whitelisted by the impl.
    #[allow(clippy::type_complexity)]
    async fn list_app_datasets(
        &self,
        workspace_id: &str,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Dataset, Option<String>, Option<String>)>, i64)>;
    /// Atomically insert a db-kind workspace together with its bootstrap
    /// dataset in a single transaction — either both rows land or neither.
    /// Closes the crash window where `create_workspace` followed by a
    /// separate `create_dataset` could leave an `active` workspace with no
    /// default dataset (unusable, no repair path). Milvus collection
    /// provisioning still happens after this returns.
    async fn create_db_workspace(
        &self,
        workspace: &Workspace,
        dataset: &Dataset,
    ) -> Result<()>;
    async fn get_workspace(&self, id: &str) -> Result<Option<Workspace>>;
    /// Cursor-paginated list of active workspaces for an account.
    /// `after` = id of the last item from the previous page (None = first
    /// page). Returns up to `limit` items plus a `has_more` flag —
    /// implementation internally fetches `limit + 1` to detect overflow
    /// and drops the extra. Sort order is `id ASC` (UUID lexicographic).
    async fn list_workspaces(
        &self,
        account_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Workspace>, bool)>;
    /// Return the IDs of all active workspaces across all accounts.
    /// Used by the reconciler to iterate workspaces during drift detection.
    async fn list_active_workspace_ids(&self) -> Result<Vec<String>>;

    /// Admin surface: every active workspace across all accounts, each with
    /// its active dataset + key counts and creator identity, sorted newest
    /// first. Powers the admin dashboard's cross-tenant overview — the only
    /// list that ignores account boundaries. Tuple is
    /// `(workspace, dataset_count, key_count, creator, creator_name)`.
    #[allow(clippy::type_complexity)]
    async fn list_all_workspaces_with_counts(
        &self,
    ) -> Result<Vec<(Workspace, i64, i64, Option<String>, Option<String>)>>;
    async fn delete_workspace(&self, id: &str) -> Result<()>;
    /// Hard-delete a workspace row by id. Used only for rollback during
    /// db-kind workspace provisioning (see routes/account.rs::create_workspace).
    /// Normal soft-delete uses `delete_workspace` (UPDATE status='archived').
    async fn hard_delete_workspace(&self, id: &str) -> Result<()>;

    // dataset (db-kind workspace only)
    async fn create_dataset(&self, dataset: &Dataset) -> Result<()>;
    /// Cursor-paginated list of active datasets within a workspace.
    /// Same contract as `list_workspaces` — sort by `id ASC`, returns
    /// `(items, has_more)` with `items.len() <= limit`.
    async fn list_active_datasets(
        &self,
        workspace_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Dataset>, bool)>;
    async fn get_active_dataset_by_name(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<Dataset>>;
    /// Soft-delete a dataset by name. Returns `true` if a row was updated,
    /// `false` if no active row matched (handler maps to 404).
    ///
    /// **Caller contract**: must never be invoked with
    /// `name == validate::DEFAULT_DATASET` — that dataset is the implicit
    /// fallback for vector API calls and archiving it would silently break
    /// every caller that omits the `dataset` field. Routes/datasets.rs
    /// enforces this; no SQL-layer guard (MySQL CHECK on string equality
    /// is awkward) — keep handler discipline.
    async fn archive_dataset(&self, workspace_id: &str, name: &str) -> Result<bool>;
    /// Hard-delete all dataset rows for a workspace. Used only for rollback
    /// during db-kind workspace provisioning.
    async fn hard_delete_datasets_for_workspace(&self, workspace_id: &str) -> Result<()>;

    // workspace key
    async fn create_workspace_key(&self, key: &WorkspaceKey) -> Result<()>;
    async fn get_workspace_key_by_hash(&self, key_hash: &str) -> Result<Option<WorkspaceKey>>;
    async fn list_workspace_keys(&self, workspace_id: &str) -> Result<Vec<WorkspaceKey>>;
    /// Revoke a key, scoped to its workspace so a caller can't revoke a key by
    /// bare id under a workspace they don't own (mirrors `get_workspace_key_token`).
    async fn revoke_workspace_key(&self, id: &str, workspace_id: &str) -> Result<()>;
}

// ── Embedding Service ──────────────────────────────────

#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}

// ── Vector Workspace Store ────────────────────────────
//
// Pinecone-style collection lifecycle for db-kind workspaces. Kept separate
// from `VectorStore` (which serves fs-side chunks/summaries) so the file-API
// trait stays focused. Stage 4 will extend this trait with upsert/search/
// query/delete as the data plane lands.

#[async_trait]
pub trait VectorWorkspaceStore: Send + Sync {
    /// Provision the per-workspace default Milvus collection with the
    /// §2.2 schema and all v0 indexes; loads on success. Idempotent: a
    /// duplicate-create call returns the same name without error.
    async fn create_vector_collection(
        &self,
        workspace_id: &str,
        dim: u32,
    ) -> Result<String>;

    /// Drop a Milvus collection by name. Idempotent: not-exists returns Ok.
    /// Used both for rollback (failed provisioning) and admin GC.
    async fn drop_collection(&self, name: &str) -> Result<()>;

    /// Upsert (insert or replace by PK) a batch of records into the
    /// workspace's default collection. The caller has already:
    ///   - validated all inputs (validate.rs)
    ///   - built composite `pk = "{dataset}:{id}"`
    ///   - computed dense `vector` (sparse_vector is auto-generated by the
    ///     collection's BM25 function — do NOT include in the payload)
    ///   - filled all default fields (category/tags/status/timestamps)
    /// Returns server-side `commit_ts` (ms epoch) — used for read-your-writes.
    async fn upsert_records(
        &self,
        workspace_id: &str,
        records: &[UpsertRecord],
    ) -> Result<i64>;

    /// Insert a batch WITHOUT dedup (Milvus `/entities/insert`). Unlike
    /// `upsert_records`, a repeated PK inserts a duplicate row — caller must
    /// guarantee uniqueness (id-less UUID, or the `write_mode=insert`
    /// contract). Non-idempotent: the impl MUST NOT auto-retry (a replay
    /// after commit-then-timeout duplicates). Returns server-now `commit_ts`.
    async fn insert_records(
        &self,
        workspace_id: &str,
        records: &[UpsertRecord],
    ) -> Result<i64>;

    /// Search within a single dataset of one workspace. `query` selects the
    /// mode and carries its data (`Semantic`/`Hybrid` need the dense vector,
    /// `Fulltext` the raw text). The impl auto-appends `status == "active"`
    /// and `dataset == "<name>"` to the Milvus filter (v0 has no cross-dataset
    /// search; the active-only default is *not* overridable by `extra_filter`),
    /// and for hybrid applies that base to BOTH sub-requests.
    /// `extra_filter` (when `Some`) is the caller's Filter DSL already
    /// translated to a Milvus expression string; impl AND-merges it with
    /// the base filter. Returns at most `top_k` hits. Each hit's `score_type`
    /// reflects the mode (`cosine`/`bm25`/`rrf`); scores are not comparable
    /// across modes. Hybrid failures propagate as errors (no silent fallback).
    async fn search_vectors(
        &self,
        workspace_id: &str,
        dataset: &str,
        query: VectorSearchQuery<'_>,
        top_k: usize,
        extra_filter: Option<&str>,
        // Projection whitelist (already validated). `None` → all fields;
        // `Some(&[..])` → only `id` + the listed fields. `id`/`score`
        // are always returned regardless.
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorSearchHit>>;

    /// Look up records by composite PK. Order is not preserved (Milvus may
    /// return rows in any order); caller can re-sort by `pk` if needed.
    /// PKs missing from the collection are silently absent from the result.
    async fn query_vectors_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorRecordHit>>;

    /// Delete records by composite PK. Returns the number of PKs submitted
    /// to Milvus (NOT the count actually deleted — Milvus REST does not
    /// surface that). Callers needing a "real deleted count" must
    /// query-before-delete on their own.
    async fn delete_vectors_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
    ) -> Result<usize>;

    /// Count active records in one dataset of a workspace's collection
    /// (admin/stats surface). Uses Milvus `count(*)` over the
    /// `dataset == "<name>" && status == "active"` filter. Errors (e.g. the
    /// collection not yet provisioned) propagate — the admin handler decides
    /// whether to surface them as "unknown" rather than failing the page.
    async fn count_vectors(&self, workspace_id: &str, dataset: &str) -> Result<i64>;
}

// ── LLM Service ────────────────────────────────────────

/// One message in an OpenAI-style chat transcript. Domain type owned by
/// veda-core so `AnswerService` and mocks never touch provider wire formats;
/// `veda-pipeline`'s `LlmProvider` maps these onto its private request DTOs.
#[derive(Debug, Clone)]
pub struct ChatMsg {
    /// "system" | "user" | "assistant" | "tool".
    pub role: String,
    pub content: String,
    /// Non-empty only on an assistant message echoing a tool-call round.
    pub tool_calls: Vec<ToolCall>,
    /// `Some` only when `role == "tool"` — links the result to its call.
    pub tool_call_id: Option<String>,
}

impl ChatMsg {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }
    /// The assistant turn that requested these tool calls (content empty).
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls,
            tool_call_id: None,
        }
    }
    /// One tool result, answering the call with `id`.
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }
    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// One tool invocation requested by the model. `arguments` is the raw JSON
/// string exactly as produced (possibly malformed — executors must treat it
/// as untrusted input and fail soft).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Declaration of one callable tool (OpenAI "function" shape). `parameters`
/// is a JSON Schema object.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// One item on a `chat_stream` channel. `Content` deltas arrive as generated;
/// `ToolCalls` is emitted at most once, fully assembled, at end of stream —
/// it is always the final item when present.
#[derive(Debug, Clone)]
pub enum ChatStreamItem {
    Content(String),
    ToolCalls(Vec<ToolCall>),
}

#[async_trait]
pub trait LlmService: Send + Sync {
    /// Worker-driven L0/L1 summary generation (flat prompt, no tools).
    async fn summarize(&self, content: &str, max_tokens: usize) -> Result<String>;
    /// Streaming chat with optional tool calling, for the agentic answer
    /// path. `tools` empty → the `tools` field is omitted on the wire (used
    /// by the forced final round). Channel contract mirrors the old
    /// `complete_stream`: the outer `Result` is the connection attempt
    /// (retryable by the caller — nothing has streamed yet); an `Err` item
    /// means the stream broke mid-generation (not retryable once content
    /// was forwarded). A channel that closes without a `ToolCalls` item is a
    /// pure content round. Dropping the receiver cancels the request.
    async fn chat_stream(
        &self,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
        max_tokens: usize,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamItem>>>;
}

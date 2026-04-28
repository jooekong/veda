use async_trait::async_trait;
use veda_types::*;

// ── Metadata Store ─────────────────────────────────────

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn ping(&self) -> Result<()>;
    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn list_dentries(&self, workspace_id: &str, parent_path: &str) -> Result<Vec<Dentry>>;
    async fn list_dentries_under(
        &self,
        workspace_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<Dentry>> {
        let mut all = Vec::new();
        let mut queue = vec![path_prefix.to_string()];
        while let Some(dir) = queue.pop() {
            let children = self.list_dentries(workspace_id, &dir).await?;
            for c in &children {
                if c.is_dir {
                    queue.push(c.path.clone());
                }
            }
            all.extend(children);
        }
        Ok(all)
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
    async fn get_file_chunks(
        &self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>>;
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
    async fn get_dentry_paths_by_file_ids(
        &self,
        workspace_id: &str,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut map = std::collections::HashMap::new();
        for fid in file_ids {
            if let Some(p) = self.get_dentry_path_by_file_id(workspace_id, fid).await? {
                map.insert(fid.clone(), p);
            }
        }
        Ok(map)
    }
    async fn query_fs_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FsEvent>>;
    async fn storage_stats(&self, workspace_id: &str) -> Result<StorageStats>;
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
    async fn upsert_summary(&self, summary: &FileSummary) -> Result<()>;
    async fn delete_summary_by_file(&self, file_id: &str) -> Result<()>;
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
    async fn list_dentries_under(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<Dentry>>;
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
    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()>;
    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()>;
    /// Delete chunks with `chunk_index >= from_chunk_index`. Used by incremental
    /// append to prune the trailing chunk(s) before re-inserting rebalanced ones.
    async fn delete_file_chunks_from(&mut self, file_id: &str, from_chunk_index: i32)
        -> Result<()>;
    /// Return the chunk with the largest `chunk_index`, or `None` for an
    /// empty/inline file. Used to seed the incremental append path.
    async fn get_last_file_chunk(&mut self, file_id: &str) -> Result<Option<FileChunk>>;

    // outbox
    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()>;

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
    async fn delete_chunks(&self, workspace_id: &str, file_id: &str) -> Result<()>;
    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;

    async fn upsert_summaries(&self, summaries: &[SummaryWithEmbedding]) -> Result<()>;
    async fn delete_summary(&self, workspace_id: &str, id: &str) -> Result<()>;
    async fn search_summaries(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;

    async fn init_collections(&self, embedding_dim: u32) -> Result<()>;
}

// ── Task Queue ─────────────────────────────────────────

#[async_trait]
pub trait TaskQueue: Send + Sync {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()>;
    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>>;
    async fn complete(&self, task_id: i64) -> Result<()>;
    async fn fail(&self, task_id: i64, error: &str) -> Result<()>;
    async fn has_pending_event(
        &self,
        event_type: OutboxEventType,
        workspace_id: &str,
        payload_key: &str,
        payload_value: &str,
    ) -> Result<bool>;
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

    // api key
    async fn create_api_key(&self, key: &ApiKeyRecord) -> Result<()>;
    async fn get_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRecord>>;
    async fn list_api_keys(&self, account_id: &str) -> Result<Vec<ApiKeyRecord>>;
    async fn revoke_api_key(&self, id: &str) -> Result<()>;

    // workspace
    async fn create_workspace(&self, workspace: &Workspace) -> Result<()>;
    async fn get_workspace(&self, id: &str) -> Result<Option<Workspace>>;
    async fn list_workspaces(&self, account_id: &str) -> Result<Vec<Workspace>>;
    async fn delete_workspace(&self, id: &str) -> Result<()>;

    // workspace key
    async fn create_workspace_key(&self, key: &WorkspaceKey) -> Result<()>;
    async fn get_workspace_key_by_hash(&self, key_hash: &str) -> Result<Option<WorkspaceKey>>;
    async fn list_workspace_keys(&self, workspace_id: &str) -> Result<Vec<WorkspaceKey>>;
    async fn revoke_workspace_key(&self, id: &str) -> Result<()>;
}

// ── Embedding Service ──────────────────────────────────

#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}

// ── LLM Service (for L0/L1 summary generation) ────────

#[async_trait]
pub trait LlmService: Send + Sync {
    async fn summarize(&self, content: &str, max_tokens: usize) -> Result<String>;
}

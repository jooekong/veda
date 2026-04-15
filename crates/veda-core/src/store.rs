use async_trait::async_trait;
use veda_types::*;

// ── Metadata Store ─────────────────────────────────────

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn list_dentries(&self, workspace_id: &str, parent_path: &str)
        -> Result<Vec<Dentry>>;
    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>>;
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
    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>>;
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
    async fn delete_dentries_under(
        &mut self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<u64>;
    async fn rename_dentry(
        &mut self,
        workspace_id: &str,
        old_path: &str,
        new_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<()>;

    // file ops
    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>>;
    async fn insert_file(&mut self, file: &FileRecord) -> Result<()>;
    async fn update_file_revision(
        &mut self,
        file_id: &str,
        revision: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
    ) -> Result<()>;
    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32>;
    async fn increment_ref_count(&mut self, file_id: &str) -> Result<()>;
    async fn delete_file(&mut self, file_id: &str) -> Result<()>;

    // content ops
    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()>;
    async fn delete_file_content(&mut self, file_id: &str) -> Result<()>;
    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()>;
    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()>;

    // outbox
    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()>;

    // fs event
    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()>;

    async fn commit(self: Box<Self>) -> Result<()>;
    async fn rollback(self: Box<Self>) -> Result<()>;
}

// ── Vector Store ───────────────────────────────────────

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert_chunks(&self, chunks: &[ChunkWithEmbedding]) -> Result<()>;
    async fn delete_chunks(&self, workspace_id: &str, file_id: &str) -> Result<()>;
    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;
    async fn hybrid_search(&self, req: &HybridSearchRequest) -> Result<Vec<SearchHit>>;

    async fn init_collections(&self, embedding_dim: u32) -> Result<()>;
}

// ── Task Queue ─────────────────────────────────────────

#[async_trait]
pub trait TaskQueue: Send + Sync {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()>;
    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>>;
    async fn complete(&self, task_id: i64) -> Result<()>;
    async fn fail(&self, task_id: i64, error: &str) -> Result<()>;
}

// ── Embedding Service ──────────────────────────────────

#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}

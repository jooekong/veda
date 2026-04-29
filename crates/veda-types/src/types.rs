use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Enums ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Active,
    Suspended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    Active,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPermission {
    Read,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageType {
    Inline,
    Chunked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Text,
    Pdf,
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxEventType {
    ChunkSync,
    ChunkDelete,
    CollectionSync,
    SummarySync,
    DirSummarySync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Processing,
    Completed,
    Failed,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEventType {
    Create,
    Update,
    Delete,
    Move,
}

impl FsEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Move => "move",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Hybrid,
    Semantic,
    Fulltext,
}

impl Default for SearchMode {
    fn default() -> Self {
        Self::Hybrid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionType {
    Structured,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionStatus {
    Active,
    Deleting,
}

// ── Control Plane ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    pub status: AccountStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub status: WorkspaceStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub account_id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub status: KeyStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceKey {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub permission: KeyPermission,
    pub status: KeyStatus,
    pub created_at: DateTime<Utc>,
}

// ── File System ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dentry {
    pub id: String,
    pub workspace_id: String,
    pub parent_path: String,
    pub name: String,
    pub path: String,
    pub file_id: Option<String>,
    pub is_dir: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub workspace_id: String,
    pub size_bytes: i64,
    pub mime_type: String,
    pub storage_type: StorageType,
    pub source_type: SourceType,
    pub line_count: Option<i32>,
    pub checksum_sha256: String,
    pub revision: i32,
    pub ref_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub file_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChunk {
    pub file_id: String,
    pub chunk_index: i32,
    pub start_line: i32,
    /// Lines contained in this chunk (number of '\n' in `content`).
    pub line_count: i32,
    /// Byte length of `content` — the exact bytes stored, used to reconstruct
    /// size / byte offsets without re-scanning the text.
    pub byte_len: i32,
    /// sha256 of `content.as_bytes()`. Enables append to skip re-hashing
    /// chunks whose content did not change.
    pub chunk_sha256: String,
    pub content: String,
}

// ── Outbox ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub id: i64,
    pub workspace_id: String,
    pub event_type: OutboxEventType,
    pub payload: serde_json::Value,
    pub status: OutboxStatus,
    pub retry_count: i32,
    pub max_retries: i32,
    pub available_at: DateTime<Utc>,
    pub lease_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// ── Collection ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub collection_type: CollectionType,
    pub schema_json: serde_json::Value,
    pub embedding_source: Option<String>,
    pub embedding_dim: Option<i32>,
    pub status: CollectionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CollectionSchema {
    pub fn milvus_name(&self) -> String {
        format!("veda_coll_{}", self.id.replace('-', "_"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDefinition {
    pub name: String,
    #[serde(rename = "type", alias = "field_type")]
    pub field_type: String,
    #[serde(default)]
    pub index: bool,
}

// ── Summary (L0/L1/L2 tiered context) ─────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStatus {
    Pending,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    Abstract,
    Overview,
    Full,
}

impl Default for DetailLevel {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSummary {
    pub id: String,
    pub workspace_id: String,
    pub file_id: Option<String>,
    pub dentry_id: Option<String>,
    pub l0_abstract: String,
    pub l1_overview: String,
    pub status: SummaryStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Search ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub workspace_id: String,
    pub query: String,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    pub path_prefix: Option<String>,
    /// When set (e.g. by SearchService for semantic mode), vector backends use this for ANN search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector: Option<Vec<f32>>,
}

fn default_search_limit() -> usize {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub file_id: String,
    pub chunk_index: Option<i32>,
    pub content: String,
    pub score: f32,
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l0_abstract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_overview: Option<String>,
}

// ── Vector / Embedding ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChunkWithEmbedding {
    pub id: String,
    pub workspace_id: String,
    pub file_id: String,
    pub chunk_index: i32,
    pub content: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SummaryWithEmbedding {
    pub id: String,
    pub workspace_id: String,
    pub summary_type: String,
    pub content: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SemanticChunk {
    pub index: i32,
    pub content: String,
}

// ── FS Events ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEvent {
    pub id: i64,
    pub workspace_id: String,
    pub event_type: FsEventType,
    pub path: String,
    pub file_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ── Storage Stats ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStats {
    pub total_files: i64,
    pub total_directories: i64,
    /// Logical bytes: sum of file sizes as seen by the user.
    /// Deduped files are counted once per dentry (copy = double-counted).
    pub total_bytes: i64,
}

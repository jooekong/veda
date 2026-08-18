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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Fs,
    Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetStatus {
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
    // Wire/DB value is `readwrite` (no underscore) — see
    // `POST /v1/workspaces/{id}/keys` and the `veda_workspace_keys.permission`
    // column DEFAULT. Override `rename_all = "snake_case"` to keep that.
    #[serde(rename = "readwrite")]
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageType {
    Inline,
    Chunked,
    /// Raw bytes in `veda_file_blobs` (binary: pdf/image/jar/...). Not UTF-8 text.
    Blob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Text,
    Pdf,
    /// Word document (.doc legacy binary or .docx OOXML): stored as blob,
    /// text-extracted for indexing like Pdf.
    Word,
    Image,
    /// Opaque binary (jar/exe/zip/...): stored as blob, not indexed.
    Binary,
}

impl SourceType {
    /// Blob types whose text layer is extracted and indexed via ExtractSync.
    pub fn is_extractable(self) -> bool {
        matches!(self, SourceType::Pdf | SourceType::Word)
    }
}

/// The OOXML .docx mime, as detected by `infer` from magic bytes. Shared by
/// mime→SourceType routing (veda-core) and the extractor dispatch
/// (veda-pipeline) so the two can never drift apart.
pub const MIME_DOCX: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
/// The legacy Word 97-2003 .doc mime.
pub const MIME_DOC: &str = "application/msword";
/// Generic OLE compound file: what `infer` reports when it cannot sub-type an
/// OLE container (its strict CFB probe rejects spec-violating writers like
/// macOS textutil). Write-time detection sniffs these for the `WordDocument`
/// stream and normalizes genuine .doc files to [`MIME_DOC`]; the extractor
/// still accepts this mime for rows stored before that normalization.
pub const MIME_OLE_STORAGE: &str = "application/x-ole-storage";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxEventType {
    ChunkSync,
    ChunkDelete,
    SummarySync,
    DirSummarySync,
    /// Extract text from a binary blob (pdf) → embed into Milvus for search.
    ExtractSync,
    /// Heal the memory vector index after a synchronous Milvus write failed
    /// (save/update/delete already committed to MySQL). Payload:
    /// {memory_id, op: "upsert"|"delete", scope_type, scope_id}.
    MemorySync,
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
    /// Background summary worker just finished a directory-level summary.
    /// `path` is the parent directory whose `.abstract` / `.overview`
    /// sidecars are now valid. Carries no `file_id`. FUSE consumers use
    /// this to clear their per-dir sidecar-miss cache without disturbing
    /// read/attr/dir caches (no actual file contents changed).
    SummaryReady,
}

impl FsEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::SummaryReady => "summary_ready",
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

/// Write semantics for `POST /v1/vectors/upsert`. Default `Upsert` is the
/// idempotent dedup-by-id path; `Insert` skips Milvus's dedup+delete for ~3x
/// throughput but does NOT dedup — a repeated id inserts a duplicate row
/// (caller guarantees id uniqueness). See docs/archive/plans/vector-write-mode-plan.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    Upsert,
    Insert,
}

impl Default for WriteMode {
    fn default() -> Self {
        Self::Upsert
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
}

// ── Control Plane ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    /// Platform business-app id. `Some` for app_id-mode accounts (platform
    /// creates them with no email/password); `None` for email/anonymous
    /// accounts. Unique when set.
    pub app_id: Option<String>,
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
    pub kind: WorkspaceKind,
    pub app_id: Option<String>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub status: DatasetStatus,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fully-resolved record ready for Milvus upsert. Handler builds this from
/// the user-facing `NewRecord` after filling defaults, computing pk via
/// `validate::build_pk(dataset, id)`, and embedding `text`.
/// `sparse_vector` is intentionally absent — Milvus's BM25 function
/// computes it from `text` on insert. `status` is hardcoded `"active"`
/// on write inside `milvus.rs` (v0 has no soft-delete via API status).
#[derive(Debug, Clone)]
pub struct UpsertRecord {
    pub pk: String,
    pub id: String,
    pub dataset: String,
    pub category: String,
    pub tags: Vec<String>,
    pub text: String,
    pub vector: Vec<f32>,
    pub meta: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Hit returned from `/v1/vectors/search`. Score is the dense ANN distance
/// (COSINE — higher is more similar). Distinct from `VectorRecordHit`
/// which omits score (Codex Stage 4.3 review suggestion to keep the wire
/// contract clean rather than serialize `score: null`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub id: String,
    // Projectable fields: `None` when the caller's `output_fields` excluded
    // them, and `skip_serializing_if` keeps them out of the wire JSON. With
    // no `output_fields` (default), all are `Some` — identical wire shape to
    // before projection landed. `id`/`score` are always returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    pub score: f32,
    /// What `score` means: `"cosine"` (semantic ANN, ~[0,1]), `"bm25"`
    /// (full-text, ~[0,30]), or `"rrf"` (hybrid fusion, ~[0,0.033]). Scores
    /// are NOT comparable across types — clients must read this before
    /// reasoning about magnitude. Defaults to `"cosine"` when absent so an
    /// older payload (semantic-only era) deserializes to its true meaning.
    #[serde(default = "default_vector_score_type")]
    pub score_type: String,
}

fn default_vector_score_type() -> String {
    "cosine".to_string()
}

/// What to search for in a db-workspace `search_vectors` call. Carries exactly
/// the data each mode needs, so illegal combinations (e.g. full-text with a
/// dense vector, or semantic without one) are unrepresentable. The handler
/// builds this after deciding whether to embed: semantic/hybrid embed the
/// query, fulltext does not.
#[derive(Debug, Clone, Copy)]
pub enum VectorSearchQuery<'a> {
    /// Dense ANN over `vector` (COSINE). Maps to `score_type = "cosine"`.
    Semantic { vector: &'a [f32] },
    /// BM25 full-text over `sparse_vector`, using the raw query string.
    /// Maps to `score_type = "bm25"`.
    Fulltext { text: &'a str },
    /// Dense + BM25 fused by RRF. Maps to `score_type = "rrf"`.
    Hybrid { vector: &'a [f32], text: &'a str },
}

/// Hit returned from `/v1/vectors/query` (by id). No score — this is a
/// direct lookup, not a ranked match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorRecordHit {
    pub id: String,
    // Same projection semantics as VectorSearchHit (minus score).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub account_id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub status: KeyStatus,
    /// Governance label identifying the business app this token represents.
    /// NOT a security boundary — workspace access is gated by
    /// `allowed_workspaces` + `workspace.kind`. Used for audit / oncall.
    pub app_id: Option<String>,
    /// Workspace scope. `None` = unrestricted (account-wide); `Some(list)` =
    /// token can only access workspaces whose `id` is in the list.
    pub allowed_workspaces: Option<Vec<String>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceKey {
    pub id: String,
    pub workspace_id: String,
    /// Denormalized from the owning workspace at key-creation time. Lets db
    /// data-plane auth verify the account is still active with a single
    /// `JOIN veda_accounts`, no workspace round-trip.
    pub account_id: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub permission: KeyPermission,
    pub status: KeyStatus,
    /// Denormalized from the owning workspace at creation. Lets auth route
    /// fs/db without re-fetching the workspace. Immutable — a workspace's
    /// kind never changes after creation.
    pub kind: WorkspaceKind,
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
    /// Hash of the content at the time of the last successful Milvus embed.
    /// NULL for files never embedded. Worker compares this against
    /// `checksum_sha256` to skip redundant embedding when content is unchanged.
    pub last_embedded_content_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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

/// Extracted full text of an extractable blob (pdf/word), written by the
/// ExtractSync worker so read paths serve text without re-parsing the blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileExtract {
    pub file_id: String,
    pub content: String,
    /// `checksum_sha256` of the blob this text came from. Readers must compare
    /// it against the file's current checksum — a mismatch means the blob was
    /// rewritten and re-extraction is still in flight (treat as absent).
    pub source_sha256: String,
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
    /// When set (e.g. by SearchService for semantic mode), vector backends use this for ANN search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector: Option<Vec<f32>>,
    /// Scope pushdown for path_prefix searches. The chunk collection
    /// filters on `file_id`, the summary collection on `id` (which holds
    /// file_ids for file summaries and dentry_ids for directory
    /// summaries). Retrieval then ranks *inside* the subtree instead of
    /// fetching a global top-K and post-filtering — a small directory in
    /// a large workspace would otherwise be starved out of the candidate
    /// window entirely. `Some(vec![])` must not reach the stores; the
    /// service returns empty upfront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_filter: Option<Vec<String>>,
}

fn default_search_limit() -> usize {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Internal join key — never sent to clients. Server-side resolves
    /// `path` from it; once that's done the field carries no information
    /// the caller would have permission to act on. `default` lets
    /// downstream code reuse this struct to *deserialize* the public
    /// search response (where the field is absent) without errors.
    #[serde(skip_serializing, default)]
    pub file_id: String,
    /// Internal counting key, populated alongside `path` during batch
    /// resolution — never serialized. Access stats aggregate on dentry_id
    /// (stable across overwrite and rename, unlike `file_id`/`path`).
    /// Stays `None` for hits that don't resolve (detached file_ids,
    /// directory-summary hits), which is exactly the skip signal.
    #[serde(skip_serializing, default)]
    pub dentry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<i32>,
    pub content: String,
    pub score: f32,
    /// What `score` actually means: "rrf" (hybrid fusion, ~[0, 0.033]),
    /// "bm25" (raw BM25, ~[0, 30]), or "cosine" (cosine similarity,
    /// ~[0, 1]). Scores are NOT comparable across types — agents/clients
    /// must read this field before reasoning about magnitude.
    #[serde(default = "default_score_type")]
    pub score_type: String,
    /// None means the backend couldn't resolve a path for this hit
    /// (detached file_id with no live dentry). Clients should treat
    /// it as "unknown", not "/" — that's why the key is omitted
    /// rather than emitted as null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l0_abstract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_overview: Option<String>,
}

fn default_score_type() -> String {
    // Used only by serde when older payloads omit the field.
    "unknown".to_string()
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

// ── Memory ─────────────────────────────────────────────
// Atomic memories with ownership partitioning (design: docs/design/agent-memory.md,
// build plan: docs/plans/agent-memory-m1.md). MySQL is the single source of
// truth; Milvus holds an index-only copy (id + scope scalars + vector).

/// Wire-level scope selector (save/search inputs). Resolved server-side
/// into a storage domain — clients never pass scope ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MemoryScope {
    /// Operator's personal domain (the default). Falls back to the agent's
    /// own domain when no operator identity resolves (shared key).
    #[default]
    #[serde(rename = "mine")]
    Mine,
    /// Current workspace's team domain.
    #[serde(rename = "team")]
    Team,
    /// The agent's own domain — meaningful for shared/unattended agents;
    /// identical to Mine under M1's key-only identity.
    #[serde(rename = "self")]
    SelfScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeType {
    /// Team domain — scope_id is a workspace id.
    Workspace,
    /// Personal domain — scope_id is a principal id.
    Principal,
}

impl MemoryScopeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Principal => "principal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(Self::Workspace),
            "principal" => Some(Self::Principal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Decision,
    Procedure,
    /// Profile conclusion induced from other memories (M3). Ranked lower at
    /// read time by kind alone — no confidence column by design.
    Derived,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Procedure => "procedure",
            Self::Derived => "derived",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Human,
    Agent,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalSource {
    Gateway,
    Wecom,
    Key,
}

impl PrincipalSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Wecom => "wecom",
            Self::Key => "key",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: i64,
    pub scope_type: MemoryScopeType,
    pub scope_id: String,
    /// Personal domain only: project note carries the workspace it belongs
    /// to, portable preference stays None. Always None for team memories.
    pub origin_workspace_id: Option<String>,
    pub topic: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    pub content_hash: String,
    pub source_ref: Option<serde_json::Value>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Bumped on retrieval hit (ranking signal), NOT on edit — edit audit
    /// is updated_at.
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub id: String,
    pub kind: PrincipalKind,
    pub source: PrincipalSource,
    pub external_id: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Insert payload — id/timestamps assigned by the store.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub scope_type: MemoryScopeType,
    pub scope_id: String,
    pub origin_workspace_id: Option<String>,
    pub topic: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    pub content_hash: String,
    pub source_ref: Option<serde_json::Value>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_by: String,
}

/// Outcome of an insert against UNIQUE(scope, content_hash): exact
/// duplicates return the existing row so save stays idempotent (a retried
/// save after a partial failure must not error).
#[derive(Debug, Clone)]
pub enum MemoryInsert {
    Inserted(Memory),
    Duplicate(Memory),
}

/// Partial update. None = keep. Content and content_hash travel together
/// (service recomputes the hash). Clearing topic/expires_at is not
/// supported in M1 — delete and re-save.
#[derive(Debug, Clone, Default)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub content_hash: Option<String>,
    pub topic: Option<String>,
    pub source_ref: Option<serde_json::Value>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Read-side domain filter. Every memory read goes through a query carrying
/// one of these — the single scope-filtered primitive (design §4.1); no
/// bypass queries.
#[derive(Debug, Clone)]
pub enum MemoryScopeFilter {
    /// Exactly one domain — save's neighbor search, scoped search.
    Scope {
        scope_type: MemoryScopeType,
        scope_id: String,
    },
    /// The context union (design §8): team domain of `workspace_id` plus
    /// the personal domain of `principal_id` restricted to
    /// origin ∈ {workspace_id, none}.
    Context {
        workspace_id: String,
        principal_id: String,
    },
}

/// Index-only Milvus row: no content, no kind — MySQL recheck is the
/// authority for everything but the vector.
#[derive(Debug, Clone)]
pub struct MemoryWithEmbedding {
    pub id: i64,
    pub scope_type: MemoryScopeType,
    pub scope_id: String,
    /// Empty string = none (Milvus VarChar has no NULL with dynamic
    /// fields disabled).
    pub origin_workspace_id: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryCandidate {
    pub id: i64,
    pub score: f32,
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{CollectionType, DetailLevel, FieldDefinition, SearchMode};

// ── Pagination ─────────────────────────────────────────

/// Cursor-pagination query string for list endpoints. The cursor is the
/// `id` of the last item returned in the previous page (opaque to the
/// caller — internally just the row's UUID). `limit` is clamped by the
/// handler (default 100, max 200).
#[derive(Debug, Deserialize, Default)]
pub struct PaginationQuery {
    pub limit: Option<u32>,
    pub after: Option<String>,
}

/// Envelope for paginated list responses. `next_cursor` is the id to pass
/// as `after` for the next page; it's `None` when `has_more` is `false`.
/// The list endpoints sort by row id (UUID, lexicographic) — order is
/// stable across requests but not human-meaningful; clients that want a
/// specific sort should resort client-side.
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T: Serialize> {
    pub items: Vec<T>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── Account ────────────────────────────────────────────

/// Two creation modes:
///   - app_id mode (platform): set `app_id`, omit email/password.
///   - email mode (console/CLI): set `email` + `password`, omit app_id.
#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub account_id: String,
    pub api_key: String,
    /// Echoed back for app_id-mode accounts; absent for email-mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub account_id: String,
    pub api_key: String,
}

/// One-shot anonymous onboarding. Mints an account with no email /
/// password (claim later), a default workspace, and both an account
/// key and a workspace key, so the CLI is fully usable after a single
/// round-trip.
#[derive(Debug, Serialize)]
pub struct AnonymousOnboardResponse {
    pub account_id: String,
    pub api_key: String,
    pub workspace_id: String,
    pub workspace_key: String,
}

/// Upgrade an anonymous account to a named one by attaching email +
/// password. The same `api_key` continues to work; the only change
/// is the account now has a recoverable identity.
#[derive(Debug, Deserialize)]
pub struct ClaimAccountRequest {
    pub email: String,
    pub password: String,
    /// Optional human-friendly name; if absent the auto-generated
    /// `anon-xxxx` is kept.
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClaimAccountResponse {
    pub account_id: String,
}

// ── Workspace ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default)]
    pub kind: crate::WorkspaceKind,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// ── File System ────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub file_id: Option<String>,
    pub is_dir: bool,
    pub size_bytes: Option<i64>,
    pub mime_type: Option<String>,
    pub revision: Option<i32>,
    pub checksum: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<i64>,
    pub mime_type: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteFileResponse {
    pub file_id: String,
    pub revision: i32,
    pub content_unchanged: bool,
}

/// File preview for the data-plane `/file` endpoint. Binary files
/// (`is_binary = true`) return empty `content` so the frontend renders a
/// download affordance instead of garbled UTF-8-lossy replacement chars.
#[derive(Debug, Serialize)]
pub struct FilePreview {
    pub path: String,
    pub size: u64,
    pub truncated: bool,
    pub mime_type: String,
    pub is_binary: bool,
    pub content: String,
}

// ── Search ─────────────────────────────────────────────

// deny_unknown_fields: reject typo'd / unsupported fields (e.g. min_score,
// which is a db-vector-plane feature) with a 4xx instead of silently
// dropping them and running an unfiltered search that looks like success.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchApiRequest {
    pub query: String,
    pub mode: Option<SearchMode>,
    pub limit: Option<usize>,
    pub path_prefix: Option<String>,
    pub detail_level: Option<DetailLevel>,
}

/// Response for `GET /v1/summary/{path}` — the L0 abstract layer, intended
/// as the cheap default. Roughly one sentence; suitable for quick context
/// previews and vector filtering. Clients that need detailed prose should
/// hit `/v1/overview/{path}` instead.
#[derive(Debug, Serialize)]
pub struct AbstractResponse {
    pub path: String,
    pub l0_abstract: String,
}

/// Response for `GET /v1/overview/{path}` — the L1 overview layer (~2k
/// tokens, structured prose). Returned only on explicit request because it
/// is significantly more expensive to send than the abstract.
#[derive(Debug, Serialize)]
pub struct OverviewResponse {
    pub path: String,
    pub l1_overview: String,
}

// ── Answer (RAG) ───────────────────────────────────────

// deny_unknown_fields mirrors SearchApiRequest: reject typo'd fields with a
// 4xx instead of silently ignoring them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerApiRequest {
    pub query: String,
    pub path_prefix: Option<String>,
    pub limit: Option<usize>,
}

/// Response for `POST /v1/answer`. `answer` is the generated text with inline
/// `[n]` markers; `citations` maps each `[n]` to a document span the reader
/// can open to verify. `estimated_context_tokens` is a conservative estimate
/// (not a real tokenizer count) of the assembled context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerApiResponse {
    pub answer: String,
    pub citations: Vec<AnswerCitation>,
    pub hit_count: usize,
    pub estimated_context_tokens: usize,
}

/// One `[n]` reference. In P0 each citation covers exactly one contiguous
/// span (`spans.len() == 1`); the field is a `Vec` so a future revision can
/// group several spans of one document under a single marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerCitation {
    pub index: usize,
    pub path: String,
    pub spans: Vec<ChunkSpan>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkSpan {
    pub start_chunk_index: i32,
    pub end_chunk_index: i32,
}

// ── Collection ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
    pub collection_type: Option<CollectionType>,
    pub fields: Vec<FieldDefinition>,
    pub embedding_source: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InsertRowsRequest {
    pub rows: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct CollectionSearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub filter: Option<serde_json::Value>,
}

// ── Grep ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub ignore_case: bool,
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GrepHit {
    pub path: String,
    pub line_no: usize,
    pub line: String,
}

// ── Vectors (db-kind workspace) ───────────────────────

/// Body for `POST /v1/vectors/upsert`. The target workspace comes from the
/// `wk_` bearer (AuthDbWorkspace). `dataset` is optional: if omitted, the
/// implicit `validate::DEFAULT_DATASET` is used (the dataset bootstrapped
/// at workspace creation).
#[derive(Debug, Deserialize)]
pub struct UpsertRequest {
    pub dataset: Option<String>,
    /// Write semantics, default `upsert` (idempotent dedup-by-id). `insert`
    /// takes the fast path (skips Milvus dedup) — caller guarantees id
    /// uniqueness; a repeated id inserts a duplicate row.
    #[serde(default)]
    pub write_mode: crate::WriteMode,
    pub records: Vec<NewRecord>,
}

/// Per-record user input. `text` is the only required field; everything
/// else has a default (see docs/vectors-merge-plan.md §2.4):
///   id        → server-generated UUID (insert semantics; no upsert dedup)
///   category  → "default"
///   tags      → []
///   meta      → {}
#[derive(Debug, Deserialize)]
pub struct NewRecord {
    /// Record identifier, unique within (workspace, dataset). Omit → server
    /// generates a UUID (insert-only; retry-prone callers must supply
    /// their own `id` to avoid duplicate writes on network retry).
    pub id: Option<String>,
    pub text: String,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct UpsertResponse {
    /// `id`s of the records written, in request order **after server-side
    /// dedupe by id** (last-wins, first-occurrence position — see
    /// `docs/api/vectors.md` "Idempotency"). May be shorter than
    /// `request.records` when the caller sent duplicate ids in one batch.
    /// For omitted-id records this surfaces the server-generated UUIDs so
    /// the caller can reference them later via query/delete.
    pub ids: Vec<String>,
    /// Server's local-time ms epoch when Milvus upsert completed.
    /// Milvus REST does not surface a true commit_ts; under synchronous
    /// upsert semantics (no outbox) this stand-in is sufficient for
    /// read-your-writes on the same server (caller can re-query immediately).
    pub commit_ts: i64,
}

#[derive(Debug, Deserialize)]
pub struct VectorSearchRequest {
    pub dataset: Option<String>,
    pub query: String,
    /// Search mode. Omitted → handler-defined default (see
    /// `routes/vectors.rs`). `semantic` = dense ANN only, `fulltext` = BM25
    /// only (skips embedding), `hybrid` = dense + BM25 fused by RRF.
    pub mode: Option<SearchMode>,
    pub top_k: Option<usize>,
    /// Relevance floor: drop hits whose `score` is below this. Only valid for
    /// `mode=semantic` (cosine) / `fulltext` (bm25) — where `score` is an
    /// interpretable similarity. Rejected (400) for `hybrid` (incl. the
    /// default), whose RRF score is a rank artifact, not relevance. Applied
    /// AFTER `top_k`, so the result may contain fewer than `top_k` hits.
    pub min_score: Option<f32>,
    pub filter: Option<VectorFilter>,
    /// Projection whitelist. `None` → all fields; `Some([...])` → only the
    /// listed projectable fields (`id`/`score` always returned). Validated
    /// against `validate::PROJECTABLE_FIELDS`; internal columns are rejected.
    #[serde(default)]
    pub output_fields: Option<Vec<String>>,
}

/// v0 Filter DSL — narrower than vss's Qdrant-style (no `should`/`must_not`,
/// only meta top-level fields). All clauses are AND-combined and merged
/// with the base filter (`dataset == "X" && status == "active"`).
#[derive(Debug, Deserialize)]
pub struct VectorFilter {
    #[serde(default)]
    pub must: Vec<FilterClause>,
}

#[derive(Debug, Deserialize)]
pub struct FilterClause {
    /// Must start with `meta.` and reference a single top-level JSON key
    /// (no nesting). v0 platform fields (dataset/status/tags/…) cannot be
    /// filtered through this DSL — search auto-applies the active+dataset
    /// scope already.
    pub field: String,
    pub op: FilterOp,
    /// `Eq` / range ops: scalar (number, string, bool).
    /// `In`: array of scalars (server expands to `OR`-chain at parse time).
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    In,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Deserialize)]
pub struct VectorQueryRequest {
    pub dataset: Option<String>,
    pub ids: Vec<String>,
    /// Projection whitelist; same semantics as `VectorSearchRequest`
    /// (`id` always returned, no `score` on this endpoint).
    #[serde(default)]
    pub output_fields: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct VectorDeleteRequest {
    pub dataset: Option<String>,
    pub ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct VectorSearchResponse {
    pub hits: Vec<crate::VectorSearchHit>,
}

#[derive(Debug, Serialize)]
pub struct VectorQueryResponse {
    pub hits: Vec<crate::VectorRecordHit>,
}

/// Number of delete markers Milvus 2.6 created for the request, mirrored
/// from REST `data.deleteCount`. v0 delete contract is "by id list", so
/// this **always equals `len(req.ids)`** regardless of physical
/// existence — Milvus's tombstone model creates one marker per id
/// expression term, not per row that previously existed. Callers who
/// need "rows that existed and were removed" must `query` first.
#[derive(Debug, Serialize)]
pub struct VectorDeleteResponse {
    pub delete_count: usize,
}

// ── Admin / Tokens ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub app_id: String,
    pub name: String,
    /// Restrict the token to a specific set of workspaces. `None` → token
    /// can access any workspace under the caller's account.
    pub allowed_workspaces: Option<Vec<String>>,
    /// Optional expiry as epoch ms. `None` → never expires.
    pub expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    pub id: String,
    /// Plaintext token — returned ONCE; never available again from the
    /// server. Caller is responsible for storing securely.
    pub token: String,
}

// ── SQL ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
}

#[cfg(test)]
mod tests {
    use crate::types::SearchHit;

    fn sample_hit(path: Option<&str>) -> SearchHit {
        SearchHit {
            file_id: "f1".into(),
            chunk_index: Some(0),
            content: "hello".into(),
            score: 0.9,
            score_type: "cosine".into(),
            path: path.map(|s| s.to_string()),
            l0_abstract: None,
            l1_overview: None,
        }
    }

    #[test]
    fn search_hit_omits_path_when_none() {
        // Detached file_id (no live dentry) → path None → JSON must not
        // contain a "path" key, so clients can distinguish "unknown" from "/".
        let json = serde_json::to_value(sample_hit(None)).unwrap();
        assert!(json.get("path").is_none(), "path key should be absent");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn search_hit_includes_path_when_some() {
        let json = serde_json::to_value(sample_hit(Some("/docs/a.md"))).unwrap();
        assert_eq!(json["path"], "/docs/a.md");
    }

    #[test]
    fn search_hit_never_serializes_file_id() {
        // file_id is an internal join key. Leaking it gives clients no
        // useful info — paths are the addressable identifier in the
        // public API — and would tempt them to reason about it.
        let json = serde_json::to_value(sample_hit(Some("/x"))).unwrap();
        assert!(json.get("file_id").is_none(), "file_id must stay server-side");
    }

    #[test]
    fn search_hit_deserializes_without_file_id() {
        // Mirrors what a CLI / SDK sees on the wire: server omits
        // file_id, so SearchHit must round-trip cleanly when it's
        // missing on the way in. Regression for Codex review.
        let wire = serde_json::json!({
            "chunk_index": 0,
            "content": "hello",
            "score": 0.9,
            "score_type": "cosine",
            "path": "/x"
        });
        let de: SearchHit = serde_json::from_value(wire).unwrap();
        assert_eq!(de.file_id, "");
        assert_eq!(de.path.as_deref(), Some("/x"));
    }
}

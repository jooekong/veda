use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{CollectionType, DetailLevel, FieldDefinition, SearchMode};

// ── Account ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub account_id: String,
    pub api_key: String,
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
}

#[derive(Debug, Serialize)]
pub struct WorkspaceTokenResponse {
    pub token: String,
    pub expires_at: DateTime<Utc>,
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

// ── Search ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
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

// ── SQL ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
}

#[derive(Debug, Serialize)]
pub struct SqlResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
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

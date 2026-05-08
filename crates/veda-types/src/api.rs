use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{CollectionType, DetailLevel, FieldDefinition, SearchHit, SearchMode};

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

#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    // None means the backend could not resolve a path for this hit (e.g. a
    // detached file_id with no live dentry). Clients should treat it as
    // "unknown", not "/".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<i32>,
    pub content: String,
    pub score: f32,
    /// "rrf" / "bm25" / "cosine". See `SearchHit::score_type` — scores from
    /// different types are not comparable.
    pub score_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l0_abstract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_overview: Option<String>,
}

impl From<SearchHit> for SearchResultItem {
    fn from(h: SearchHit) -> Self {
        Self {
            path: h.path,
            chunk_index: h.chunk_index,
            content: h.content,
            score: h.score,
            score_type: h.score_type,
            l0_abstract: h.l0_abstract,
            l1_overview: h.l1_overview,
        }
    }
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
    use super::*;
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
    fn search_result_omits_path_when_none() {
        // Detached file_id (no live dentry) → path None → JSON must not
        // contain a "path" key, so clients can distinguish "unknown" from "/".
        let item: SearchResultItem = sample_hit(None).into();
        let json = serde_json::to_value(&item).unwrap();
        assert!(json.get("path").is_none(), "path key should be absent");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn search_result_includes_path_when_some() {
        let item: SearchResultItem = sample_hit(Some("/docs/a.md")).into();
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["path"], "/docs/a.md");
    }

    #[test]
    fn search_result_round_trips_path_option() {
        let item: SearchResultItem = sample_hit(None).into();
        assert!(item.path.is_none());
        let item2: SearchResultItem = sample_hit(Some("/x")).into();
        assert_eq!(item2.path.as_deref(), Some("/x"));
    }
}

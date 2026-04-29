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
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<i32>,
    pub content: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l0_abstract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_overview: Option<String>,
}

impl From<SearchHit> for SearchResultItem {
    fn from(h: SearchHit) -> Self {
        Self {
            path: h.path.unwrap_or_default(),
            chunk_index: h.chunk_index,
            content: h.content,
            score: h.score,
            l0_abstract: h.l0_abstract,
            l1_overview: h.l1_overview,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SummaryResponse {
    pub path: String,
    pub l0_abstract: String,
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

//! Read-only admin dashboard surface (`/admin/v1/*`).
//!
//! A cross-tenant operations view: list every workspace/project on this node,
//! its data volume (fs bytes / db vector counts) + documents, and a db-vector
//! query console. Auth is a single deploy-wide bearer token
//! (`VEDA_ADMIN_TOKEN`) checked on every route — NOT the account / `wk_`
//! data-plane auth, so the admin sees across all accounts while data-plane
//! keys stay scoped to one workspace.
//!
//! **Fail-closed**: when the token is unset every handler 404s (via
//! `AdminAuth`), so an unconfigured node exposes no cross-tenant data — same
//! "don't disclose existence" posture as `/v1/metrics`. Most handlers are
//! read-only; the one exception is the db-vector upsert console
//! (`POST .../vectors/upsert`), gated by the same admin token.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;
use veda_core::store::MemoryListOrder;
use veda_store::milvus_quote;
use veda_types::api::{MemoryItem, MemoryPageResponse};
use veda_types::{
    validate, ApiResponse, KeyPermission, KeyStatus, MemoryKind, SearchMode, UpsertRecord,
    VectorSearchQuery, VedaError, Workspace, WorkspaceKind,
};

use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/v1/workspaces", get(list_workspaces))
        .route("/admin/v1/workspaces/{id}", get(get_workspace))
        .route("/admin/v1/workspaces/{id}/files", get(list_files))
        .route("/admin/v1/workspaces/{id}/file", get(read_file))
        .route("/admin/v1/workspaces/{id}/stats/docs", get(doc_stats))
        .route("/admin/v1/memories", get(list_team_memories))
        .route("/admin/v1/memories/{id}", delete(delete_team_memory))
        .route(
            "/admin/v1/workspaces/{id}/vectors/search",
            post(search_vectors),
        )
        .route(
            "/admin/v1/workspaces/{id}/vectors/upsert",
            post(upsert_vectors),
        )
}

// ── Auth ────────────────────────────────────────────────

/// Bearer-token gate for every admin route. Fail-closed:
/// - token unset (`admin_token == None` / empty) → 404, identical to a
///   non-existent route, so a node with the admin surface disabled discloses
///   nothing;
/// - token set but bearer missing/wrong → 401.
///
/// Comparison is constant-time. Unlike the data-plane extractors this checks a
/// single deploy-wide secret, NOT an account/workspace key — the admin sees
/// across all tenants by design.
struct AdminAuth;

impl FromRequestParts<Arc<AppState>> for AdminAuth {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let expected = state.admin_token.clone();
        let presented = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::to_string);
        async move {
            let Some(expected) = expected.filter(|t| !t.is_empty()) else {
                // Surface disabled — don't disclose its existence.
                return Err(StatusCode::NOT_FOUND.into_response());
            };
            match presented {
                Some(p) if crate::routes::constant_time_eq(p.as_bytes(), expected.as_bytes()) => {
                    Ok(AdminAuth)
                }
                _ => Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ApiResponse::<()>::err("UNAUTHORIZED", "unauthorized")),
                )
                    .into_response()),
            }
        }
    }
}

// ── Workspace list / detail ─────────────────────────────

#[derive(serde::Serialize)]
struct FsStats {
    total_files: i64,
    total_directories: i64,
    total_bytes: i64,
}

#[derive(serde::Serialize)]
struct AdminWorkspace {
    id: String,
    name: String,
    kind: WorkspaceKind,
    /// Platform workspace code (tenant) the project belongs to, if created via
    /// the apps gateway; null for `vk_` / anonymous workspaces.
    app_id: Option<String>,
    account_id: String,
    description: Option<String>,
    creator: Option<String>,
    creator_name: Option<String>,
    dataset_count: i64,
    key_count: i64,
    /// fs document + byte stats; null for db workspaces (no files) or if the
    /// stats query failed.
    files: Option<FsStats>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Assemble the list/detail workspace view, fetching fs byte stats for fs
/// workspaces (a single fast MySQL aggregate; skipped for db, which has no
/// dentries).
async fn build_admin_workspace(
    state: &AppState,
    ws: Workspace,
    dataset_count: i64,
    key_count: i64,
    creator: Option<String>,
    creator_name: Option<String>,
) -> AdminWorkspace {
    let files = if ws.kind == WorkspaceKind::Fs {
        state
            .meta_store
            .storage_stats(&ws.id)
            .await
            .ok()
            .map(|s| FsStats {
                total_files: s.total_files,
                total_directories: s.total_directories,
                total_bytes: s.total_bytes,
            })
    } else {
        None
    };
    AdminWorkspace {
        id: ws.id,
        name: ws.name,
        kind: ws.kind,
        app_id: ws.app_id,
        account_id: ws.account_id,
        description: ws.description,
        creator,
        creator_name,
        dataset_count,
        key_count,
        files,
        created_at: ws.created_at,
        updated_at: ws.updated_at,
    }
}

/// GET /admin/v1/workspaces — every active workspace on this node (all
/// accounts), with dataset/key counts + fs byte stats. db vector counts are
/// NOT fetched here (one Milvus round-trip per dataset) — they live on the
/// detail view.
async fn list_workspaces(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
) -> Result<Json<ApiResponse<Vec<AdminWorkspace>>>, AppError> {
    let rows = state.auth_store.list_all_workspaces_with_counts().await?;
    let mut out = Vec::with_capacity(rows.len());
    for (ws, dataset_count, key_count, creator, creator_name) in rows {
        out.push(
            build_admin_workspace(&state, ws, dataset_count, key_count, creator, creator_name)
                .await,
        );
    }
    Ok(Json(ApiResponse::ok(out)))
}

#[derive(serde::Serialize)]
struct AdminDataset {
    id: String,
    name: String,
    description: Option<String>,
    /// Live Milvus vector count; null if the count failed (collection not yet
    /// provisioned, Milvus unreachable, …) so one bad dataset doesn't sink the
    /// whole detail page.
    vector_count: Option<i64>,
    created_at: DateTime<Utc>,
}

#[derive(serde::Serialize)]
struct AdminKey {
    id: String,
    name: String,
    permission: KeyPermission,
    status: KeyStatus,
    created_at: DateTime<Utc>,
}

#[derive(serde::Serialize)]
struct AdminWorkspaceDetail {
    workspace: AdminWorkspace,
    datasets: Vec<AdminDataset>,
    keys: Vec<AdminKey>,
}

/// GET /admin/v1/workspaces/{id} — one workspace with its datasets (each
/// carrying a live Milvus vector count, db only) and keys.
async fn get_workspace(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AdminWorkspaceDetail>>, AppError> {
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    let (creator, creator_name) = state.auth_store.get_workspace_creator(&id).await?;

    // Datasets (db only — fs workspaces have none). Cap at 500; alpha
    // workspaces hold a handful.
    let (datasets_raw, _) = state.auth_store.list_active_datasets(&id, None, 500).await?;
    let is_db = ws.kind == WorkspaceKind::Db;
    let mut datasets = Vec::with_capacity(datasets_raw.len());
    for ds in datasets_raw {
        // Per-dataset live count; tolerate failures (→ null) so an absent
        // collection or Milvus blip doesn't 500 the whole page.
        let vector_count = if is_db {
            state
                .vector_workspace_store
                .count_vectors(&id, &ds.name)
                .await
                .ok()
        } else {
            None
        };
        datasets.push(AdminDataset {
            id: ds.id,
            name: ds.name,
            description: ds.description,
            vector_count,
            created_at: ds.created_at,
        });
    }

    // Keys: list all (including revoked — admin wants the full picture), but
    // count only active ones so the summary matches the list view's key_count.
    let keys_raw = state.auth_store.list_workspace_keys(&id).await?;
    let key_count = keys_raw
        .iter()
        .filter(|k| k.status == KeyStatus::Active)
        .count() as i64;
    let keys = keys_raw
        .into_iter()
        .map(|k| AdminKey {
            id: k.id,
            name: k.name,
            permission: k.permission,
            status: k.status,
            created_at: k.created_at,
        })
        .collect();

    let dataset_count = datasets.len() as i64;
    let workspace =
        build_admin_workspace(&state, ws, dataset_count, key_count, creator, creator_name).await;
    Ok(Json(ApiResponse::ok(AdminWorkspaceDetail {
        workspace,
        datasets,
        keys,
    })))
}

// ── Documents (fs) ──────────────────────────────────────

#[derive(Deserialize)]
struct FilesQuery {
    /// Directory to list, default `/`. Non-recursive (one level).
    path: Option<String>,
}

/// GET /admin/v1/workspaces/{id}/files?path=/ — list one directory level of an
/// fs workspace. db workspaces have no files → empty list.
async fn list_files(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Query(q): Query<FilesQuery>,
) -> Result<Json<ApiResponse<Vec<veda_types::api::DirEntry>>>, AppError> {
    // Confirm the workspace exists first, so a bad id 404s instead of looking
    // like a real-but-empty directory.
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    if ws.kind != WorkspaceKind::Fs {
        return Ok(Json(ApiResponse::ok(Vec::new())));
    }
    let path = q.path.as_deref().unwrap_or("/");
    // Sized variant: admin is a low-frequency display surface, the extra
    // O(subtree) aggregate per level is acceptable there.
    let entries = state.fs_service.list_dir_with_dir_sizes(&id, path).await?;
    Ok(Json(ApiResponse::ok(entries)))
}

/// GET /admin/v1/workspaces/{id}/stats/docs — the per-document heat board
/// for one fs workspace, admin view. Windowing/clamping/order semantics are
/// the shared `build_doc_stats` (same as native `/v1/stats/docs`), so the
/// two surfaces cannot drift. db workspaces return an empty board rather
/// than an error — same soft posture as `list_files`.
async fn doc_stats(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Query(q): Query<super::stats::DocStatsQuery>,
) -> Result<Json<ApiResponse<veda_types::api::DocAccessStatsResponse>>, AppError> {
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    if ws.kind != WorkspaceKind::Fs {
        return Ok(Json(ApiResponse::ok(
            veda_types::api::DocAccessStatsResponse {
                days: 0,
                items: Vec::new(),
            },
        )));
    }
    let resp = super::stats::build_doc_stats(&state, &id, &q).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Max bytes returned by the file preview. Files larger than this are truncated
/// to the leading slice — this is an admin preview, not a download endpoint.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024;

#[derive(Deserialize)]
struct FileQuery {
    /// File path to preview.
    path: String,
}

#[derive(serde::Serialize)]
struct FilePreview {
    path: String,
    /// Total file size in bytes (may exceed the returned content length).
    size: u64,
    /// True when the file exceeds MAX_PREVIEW_BYTES and `content` is only the
    /// leading slice.
    truncated: bool,
    /// UTF-8 (lossy) content of the first MAX_PREVIEW_BYTES bytes. Binary files
    /// come through with replacement chars — preview is best-effort, not a
    /// faithful binary dump.
    content: String,
}

/// GET /admin/v1/workspaces/{id}/file?path=/foo.txt — preview a file's content
/// (first MAX_PREVIEW_BYTES, UTF-8 lossy). fs workspaces only.
async fn read_file(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Json<ApiResponse<FilePreview>>, AppError> {
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    if ws.kind != WorkspaceKind::Fs {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    let (bytes, total) = state
        .fs_service
        .read_file_range(&id, &q.path, 0, MAX_PREVIEW_BYTES)
        .await?;
    Ok(Json(ApiResponse::ok(FilePreview {
        path: q.path,
        size: total,
        truncated: total > MAX_PREVIEW_BYTES,
        content: String::from_utf8_lossy(&bytes).into_owned(),
    })))
}

// ── db vector query console ─────────────────────────────

#[derive(Deserialize)]
struct AdminSearchRequest {
    /// Target dataset; default `default`.
    dataset: Option<String>,
    query: String,
    /// 1..=100, default 10.
    top_k: Option<usize>,
    /// semantic | hybrid | fulltext; default hybrid.
    mode: Option<SearchMode>,
    /// Optional exact-match category filter.
    category: Option<String>,
    /// Optional tag filters — a hit must contain EVERY listed tag.
    tags: Option<Vec<String>>,
}

/// POST /admin/v1/workspaces/{id}/vectors/search — db vector query console.
/// Mirrors the `/v1/vectors/search` data path (server-side embedding, same
/// store call) but authorized by the admin token and scoped to the path
/// workspace instead of a `wk_` key. Deliberately minimal (no filter DSL /
/// projection / min_score) — it's an ops lookup tool, not the full API.
async fn search_vectors(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(req): Json<AdminSearchRequest>,
) -> Result<Json<ApiResponse<Vec<veda_types::VectorSearchHit>>>, AppError> {
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    if ws.kind != WorkspaceKind::Db {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    validate::validate_text(&req.query)?;
    let top_k = req.top_k.unwrap_or(10).clamp(1, 100);
    let dataset = state
        .auth_store
        .get_active_dataset_by_name(
            &id,
            req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        )
        .await?
        .ok_or_else(|| VedaError::NotFound("dataset".into()))?;
    let mode = req.mode.unwrap_or(SearchMode::Hybrid);
    // Optional category + tag filter, AND-merged with the dataset/active base
    // inside the store layer.
    let extra_filter = build_scalar_filter(req.category.as_deref(), req.tags.as_deref())?;

    // Embed only for modes that need a dense vector (fulltext is BM25-only).
    let query_vector: Option<Vec<f32>> =
        if matches!(mode, SearchMode::Semantic | SearchMode::Hybrid) {
            let v = state
                .vector_embedding
                .embed(std::slice::from_ref(&req.query))
                .await?;
            Some(
                v.into_iter()
                    .next()
                    .ok_or_else(|| VedaError::EmbeddingFailed("embedded 0 vectors".into()))?,
            )
        } else {
            None
        };
    let search_query = match mode {
        SearchMode::Semantic => VectorSearchQuery::Semantic {
            vector: query_vector.as_deref().expect("semantic embeds above"),
        },
        SearchMode::Hybrid => VectorSearchQuery::Hybrid {
            vector: query_vector.as_deref().expect("hybrid embeds above"),
            text: &req.query,
        },
        SearchMode::Fulltext => VectorSearchQuery::Fulltext { text: &req.query },
    };
    let hits = state
        .vector_workspace_store
        .search_vectors(
            &id,
            &dataset.name,
            search_query,
            top_k,
            extra_filter.as_deref(),
            None,
        )
        .await?;
    Ok(Json(ApiResponse::ok(hits)))
}

/// Build a Milvus filter from optional category + tags, AND-merged. `None` when
/// both are empty. category → `category == "X"`; each tag → `array_contains(
/// tags, "Y")`. Values are validated (same rules as the data plane) then quoted,
/// so they can't break out of the expression.
fn build_scalar_filter(
    category: Option<&str>,
    tags: Option<&[String]>,
) -> Result<Option<String>, AppError> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(cat) = category.map(str::trim).filter(|c| !c.is_empty()) {
        validate::validate_category(cat)?;
        parts.push(format!("category == {}", milvus_quote(cat)));
    }
    let tag_list: Vec<String> = tags
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if !tag_list.is_empty() {
        validate::validate_tags(&tag_list)?;
        for tag in &tag_list {
            parts.push(format!("array_contains(tags, {})", milvus_quote(tag)));
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join(" && ")))
}

// ── db vector upsert console (the only mutating admin route) ──

#[derive(Deserialize)]
struct AdminUpsertRequest {
    /// Target dataset; default `default`.
    dataset: Option<String>,
    text: String,
    category: Option<String>,
    tags: Option<Vec<String>>,
}

#[derive(serde::Serialize)]
struct AdminUpsertResponse {
    id: String,
    commit_ts: i64,
}

/// POST /admin/v1/workspaces/{id}/vectors/upsert — write a single vector record
/// (text + dataset + category + tags) from the admin console. db only; embeds
/// server-side and inserts with a fresh UUID id. The only mutating admin route.
async fn upsert_vectors(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<String>,
    Json(req): Json<AdminUpsertRequest>,
) -> Result<Json<ApiResponse<AdminUpsertResponse>>, AppError> {
    let ws = state
        .auth_store
        .get_workspace(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {id}")))?;
    if ws.kind != WorkspaceKind::Db {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    validate::validate_text(&req.text)?;
    let category = req
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(validate::DEFAULT_CATEGORY)
        .to_string();
    validate::validate_category(&category)?;
    let tags: Vec<String> = req
        .tags
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    validate::validate_tags(&tags)?;
    let dataset = state
        .auth_store
        .get_active_dataset_by_name(
            &id,
            req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        )
        .await?
        .ok_or_else(|| VedaError::NotFound("dataset".into()))?;

    let rec_id = Uuid::new_v4().to_string().replace('-', "");
    let pk = validate::build_pk(&dataset.name, &rec_id)?;
    let vector = state
        .vector_embedding
        .embed(std::slice::from_ref(&req.text))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| VedaError::EmbeddingFailed("embedded 0 vectors".into()))?;
    let now_ms = Utc::now().timestamp_millis();
    let record = UpsertRecord {
        pk,
        id: rec_id.clone(),
        dataset: dataset.name,
        category,
        tags,
        text: req.text,
        vector,
        meta: json!({}),
        created_at: now_ms,
        updated_at: now_ms,
    };
    let commit_ts = state
        .vector_workspace_store
        .insert_records(&id, std::slice::from_ref(&record))
        .await?;
    // Audit the only mutating admin route: the admin token is a cross-tenant
    // "god key", so a write must be traceable (who/where/what). Text content is
    // NOT logged — only its length — to avoid leaking tenant data into logs.
    tracing::info!(
        target: "admin_audit",
        workspace_id = %id,
        dataset = %record.dataset,
        record_id = %rec_id,
        text_len = record.text.len(),
        "admin vector upsert"
    );
    Ok(Json(ApiResponse::ok(AdminUpsertResponse {
        id: rec_id,
        commit_ts,
    })))
}

#[derive(Debug, Deserialize)]
struct AdminMemoryQuery {
    /// Explicit team domain — the admin surface reaches ONLY workspace
    /// (team) memories; personal/dept domains stay owner-visible.
    workspace: String,
    kind: Option<MemoryKind>,
    /// "updated_at" (default) = wiki recency, "last_used_at" = heat view.
    order: Option<String>,
    page: Option<u32>,
    size: Option<u32>,
}

/// GET /admin/v1/memories?workspace= — team-domain cleanup list (M4a).
async fn list_team_memories(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Query(q): Query<AdminMemoryQuery>,
) -> Result<Json<ApiResponse<MemoryPageResponse>>, AppError> {
    let order = match q.order.as_deref() {
        None | Some("updated_at") => MemoryListOrder::UpdatedAt,
        Some("last_used_at") => MemoryListOrder::LastUsedAt,
        Some(other) => {
            return Err(AppError(VedaError::InvalidInput(format!(
                "unknown order '{other}' — use updated_at or last_used_at"
            ))))
        }
    };
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(50).clamp(1, 100);
    let (rows, total) = state
        .memory_service
        .admin_list_team(&q.workspace, q.kind, order, page, size)
        .await?;
    Ok(Json(ApiResponse::ok(MemoryPageResponse {
        items: rows
            .into_iter()
            .map(|m| MemoryItem::from_memory(m, None))
            .collect(),
        total,
        page,
        size,
    })))
}

#[derive(Debug, Deserialize)]
struct AdminMemoryDeleteQuery {
    workspace: String,
}

/// DELETE /admin/v1/memories/{id}?workspace= — the "admin 可清" backstop
/// (design §13). Scoped to the named workspace's team domain; audited like
/// the vector upsert (the admin token is cross-tenant).
async fn delete_team_memory(
    State(state): State<Arc<AppState>>,
    _auth: AdminAuth,
    Path(id): Path<i64>,
    Query(q): Query<AdminMemoryDeleteQuery>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    state.memory_service.admin_delete_team(&q.workspace, id).await?;
    tracing::info!(
        target: "admin_audit",
        workspace_id = %q.workspace,
        memory_id = id,
        "admin team memory delete"
    );
    Ok(Json(ApiResponse::ok(json!({ "deleted": id }))))
}

//! Platform-gateway data plane for AI Workbench projects
//! (`/v1/workspace/{workspace}/project/{id}/...`).
//!
//! Wraps veda's `wk_` data plane (vectors read/write + fs query) onto the
//! platform-gateway surface: same path prefix / cookie auth / company envelope
//! as the project/dataset/key management API (apps.rs). The AI Workbench
//! frontend calls these **without holding a `wk_`** — the gateway proves
//! identity, we resolve the project from the path and reuse the data-plane
//! core (`VectorService`, fs services).
//!
//! Mounted under the same `company_envelope` layer as apps.rs, so handlers
//! return veda's `ApiResponse<T>` and the middleware rewrites it to the
//! company shape: a `Vec<_>` (search/query/files/sql/grep) → `{data:[...],
//! page,...}`; a single struct (upsert/delete回执/file 预览) → bare object.
//!
//! **Authz**: every op — read and write — goes through external authz
//! (`authz_and_load`). The data plane exposes actual file/vector content, so we
//! don't rely on the gateway restricting paths; veda independently verifies the
//! user may act in the workspace (decided 2026-06-23, review follow-up).

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use veda_types::api::{
    DirEntry, FilePreview, GrepHit, GrepRequest, SearchApiRequest, SqlRequest, UpsertRequest,
    UpsertResponse, VectorDeleteRequest, VectorDeleteResponse, VectorQueryRequest,
    VectorSearchRequest,
};
use veda_types::{
    ApiResponse, DetailLevel, SearchHit, SearchMode, VectorRecordHit, VectorSearchHit, VedaError,
    Workspace, WorkspaceKind, WorkspaceStatus,
};

use crate::error::AppError;
use crate::platform::{authorize, GatewayUser};
use crate::routes::apps::{company_envelope, load_app_project};
use crate::state::AppState;

/// Max bytes for the file preview (mirrors the admin surface). Larger files are
/// truncated to the leading slice.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        // db 向量数据面（project kind=db）
        .route(
            "/v1/workspace/{workspace}/project/{id}/vectors/upsert",
            post(vectors_upsert),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/vectors/search",
            post(vectors_search),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/vectors/query",
            post(vectors_query),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/vectors/delete",
            post(vectors_delete),
        )
        // fs 查询数据面（project kind=fs）
        .route(
            "/v1/workspace/{workspace}/project/{id}/search",
            post(fs_search),
        )
        .route("/v1/workspace/{workspace}/project/{id}/files", get(fs_files))
        // GET = JSON preview (truncated), PUT = upload. The raw byte stream
        // lives under /file/content so a JSON-expecting preview client never
        // receives a 40MB binary by accident.
        .route(
            "/v1/workspace/{workspace}/project/{id}/file",
            get(fs_file).put(fs_upload),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/file/content",
            get(fs_download),
        )
        .route("/v1/workspace/{workspace}/project/{id}/sql", post(fs_sql))
        .route("/v1/workspace/{workspace}/project/{id}/grep", post(fs_grep))
        .route(
            "/v1/workspace/{workspace}/project/{id}/stats/docs",
            get(fs_doc_stats),
        )
        // Same company-envelope rewrite as the management surface.
        .layer(axum::middleware::from_fn(company_envelope))
        // Bulk vectors upsert runs far over axum's 2MB default (a 500-record
        // batch ≈ 40MB) — match the wk_ vectors plane's 64MB ceiling so large
        // upserts reach the structured PayloadTooLarge check, not a bare 413
        // from the body-limit layer (mirrors vectors.rs::routes).
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// External authz + resolve project, requiring active + a specific kind.
///
/// **Every** data-plane op (read and write) calls this: the gateway proves the
/// caller's identity, but veda independently verifies — via the platform authz
/// API — that the user may act in this workspace. Data-plane reads expose actual
/// file/vector/SQL content, so unlike pure path resolution we do not trust the
/// gateway to have scoped the path to the user's own workspace. `workspace-create`
/// is the workspace-scoped action veda currently checks (same as management-plane
/// writes); a finer-grained read action can be split out platform-side later.
///
/// `load_app_project` masks cross-tenant / missing both as NOT_FOUND (a probe
/// can't learn another tenant's project ids); wrong kind → WORKSPACE_KIND_MISMATCH.
async fn authz_and_load(
    state: &AppState,
    gw: &GatewayUser,
    workspace: &str,
    id: &str,
    kind: WorkspaceKind,
) -> Result<Workspace, AppError> {
    // authz before load: an unauthorized caller shouldn't learn whether the
    // project exists (mirrors apps.rs create/mint ordering).
    authorize(gw.cookie(), "workspace-create", workspace, gw.user_name()).await?;
    let ws = load_app_project(state, workspace, id).await?;
    if ws.status != WorkspaceStatus::Active {
        return Err(VedaError::NotFound(format!("project {id}")).into());
    }
    if ws.kind != kind {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    Ok(ws)
}

// ── db 向量数据面 ──────────────────────────────────────

async fn vectors_upsert(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<ApiResponse<UpsertResponse>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Db).await?;
    let resp = state.vector_service.upsert(&ws.id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn vectors_search(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<VectorSearchRequest>,
) -> Result<Json<ApiResponse<Vec<VectorSearchHit>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Db).await?;
    let resp = state.vector_service.search(&ws.id, req).await?;
    Ok(Json(ApiResponse::ok(resp.hits)))
}

async fn vectors_query(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<VectorQueryRequest>,
) -> Result<Json<ApiResponse<Vec<VectorRecordHit>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Db).await?;
    let resp = state.vector_service.query(&ws.id, req).await?;
    Ok(Json(ApiResponse::ok(resp.hits)))
}

async fn vectors_delete(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<VectorDeleteRequest>,
) -> Result<Json<ApiResponse<VectorDeleteResponse>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Db).await?;
    let resp = state.vector_service.delete(&ws.id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

// ── fs 查询数据面 ──────────────────────────────────────

async fn fs_search(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<SearchApiRequest>,
) -> Result<Json<ApiResponse<Vec<SearchHit>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let mode = req.mode.unwrap_or(SearchMode::Hybrid);
    let limit = req.limit.unwrap_or(10).min(100);
    let detail_level = req.detail_level.unwrap_or(DetailLevel::Full);
    let hits = state
        .search_service
        .search(
            &ws.id,
            &req.query,
            mode,
            limit,
            req.path_prefix.as_deref(),
            detail_level,
        )
        .await?;
    Ok(Json(ApiResponse::ok(hits)))
}

/// Document heat ranking for the AI-workbench console. Same clamping and
/// order semantics as native `GET /v1/stats/docs` (shared builder); the
/// company envelope middleware unwraps the single struct into a bare object.
async fn fs_doc_stats(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<super::stats::DocStatsQuery>,
) -> Result<Json<ApiResponse<veda_types::api::DocAccessStatsResponse>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let resp = super::stats::build_doc_stats(&state, &ws.id, &q).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

#[derive(Deserialize)]
struct FilesQuery {
    /// Directory to list, default `/`.
    path: Option<String>,
}

async fn fs_files(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<FilesQuery>,
) -> Result<Json<ApiResponse<Vec<DirEntry>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let entries = state
        .fs_service
        .list_dir(&ws.id, q.path.as_deref().unwrap_or("/"))
        .await?;
    Ok(Json(ApiResponse::ok(entries)))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn fs_file(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<FileQuery>,
) -> Result<Json<ApiResponse<FilePreview>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let preview = state
        .fs_service
        .read_file_preview(&ws.id, &q.path, MAX_PREVIEW_BYTES)
        .await?;
    Ok(Json(ApiResponse::ok(preview)))
}

/// PUT /v1/workspace/{workspace}/project/{id}/file?path=/a/b.md — upload a
/// file (create or overwrite; parents auto-created; overwrite bumps
/// `revision`). Same content sniff as the `wk_` plane (fs.rs::write_file):
/// valid UTF-8 → text (chunked/embedded/searchable), anything else → binary
/// blob (stored verbatim; PDFs get text-extracted for search, images stored
/// but not indexed). No If-Match/rev preconditions on this surface — the
/// workbench "upload" gesture is last-write-wins by design.
async fn fs_upload(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<FileQuery>,
    body: axum::body::Bytes,
) -> Result<Json<ApiResponse<veda_types::api::WriteFileResponse>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let resp = match std::str::from_utf8(&body) {
        Ok(text) => {
            state
                .fs_service
                .write_file(&ws.id, &q.path, text, None, None)
                .await?
        }
        Err(_) => {
            state
                .fs_service
                .write_blob(&ws.id, &q.path, body.to_vec(), None)
                .await?
        }
    };
    Ok(Json(ApiResponse::ok(resp)))
}

/// GET /v1/workspace/{workspace}/project/{id}/file/content?path=/a/b.md —
/// download the raw bytes (text or binary) with the stored MIME type and an
/// attachment disposition. Non-JSON, so the company envelope passes it
/// through untouched.
async fn fs_download(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<FileQuery>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let (bytes, mime) = state.fs_service.read_file_raw(&ws.id, &q.path).await?;
    let filename = q.path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("file");
    // RFC 5987 filename* carries UTF-8 names (percent-encoded → the header
    // value stays ASCII); a plain-ASCII fallback filename= keeps ancient
    // clients working.
    let ascii_fallback: String = filename
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    let encoded = percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC);
    let disposition = format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{encoded}");
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}

async fn fs_sql(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<SqlRequest>,
) -> Result<Json<ApiResponse<Vec<serde_json::Value>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    // Platform query surface is read-only.
    let batches = state.sql_engine.execute(&ws.id, true, &req.sql).await?;
    let buf = Vec::new();
    let mut writer = arrow::json::ArrayWriter::new(buf);
    for batch in &batches {
        writer
            .write(batch)
            .map_err(|e| AppError(VedaError::Storage(e.to_string())))?;
    }
    writer
        .finish()
        .map_err(|e| AppError(VedaError::Storage(e.to_string())))?;
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&writer.into_inner())
        .map_err(|e| AppError(VedaError::Storage(format!("arrow json parse failed: {e}"))))?;
    Ok(Json(ApiResponse::ok(rows)))
}

async fn fs_grep(
    State(state): State<Arc<AppState>>,
    Path((workspace, id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<GrepRequest>,
) -> Result<Json<ApiResponse<Vec<GrepHit>>>, AppError> {
    let ws = authz_and_load(&state, &gw, &workspace, &id, WorkspaceKind::Fs).await?;
    let hits = state
        .fs_service
        .grep(
            &ws.id,
            &req.pattern,
            req.path_prefix.as_deref(),
            req.ignore_case,
            req.max_results.unwrap_or(100),
        )
        .await?;
    Ok(Json(ApiResponse::ok(hits)))
}

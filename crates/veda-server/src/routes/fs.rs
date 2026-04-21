use std::sync::Arc;

use axum::extract::rejection::StringRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use veda_types::api::{FileInfo, WriteFileResponse};
use veda_types::{ApiResponse, VedaError};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

const MAX_BODY_MB: usize = 50;

pub fn routes() -> Router<Arc<AppState>> {
    let upload_routes = Router::new()
        .route("/v1/fs/{*path}", put(write_file).post(append_file))
        .layer(DefaultBodyLimit::max(MAX_BODY_MB * 1024 * 1024));

    Router::new()
        .route("/v1/fs", get(read_root).head(stat_root).delete(delete_root))
        .route(
            "/v1/fs/{*path}",
            get(read_file).head(stat_file).delete(delete_file),
        )
        .route("/v1/fs-copy", post(copy_file))
        .route("/v1/fs-rename", post(rename_file))
        .route("/v1/fs-mkdir", post(mkdir))
        .merge(upload_routes)
}

#[derive(Deserialize, Default)]
struct FsQuery {
    list: Option<String>,
    lines: Option<String>,
    stat: Option<String>,
}

async fn read_root(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Query(q): Query<FsQuery>,
) -> Result<Response, AppError> {
    if q.stat.is_some() {
        let info = state.fs_service.stat(&auth.workspace_id, "/").await?;
        return Ok(Json(ApiResponse::ok(info)).into_response());
    }
    if q.list.is_some() {
        let entries = state.fs_service.list_dir(&auth.workspace_id, "/").await?;
        return Ok(Json(ApiResponse::ok(entries)).into_response());
    }
    Err(VedaError::InvalidInput("use ?stat or ?list".into()).into())
}

async fn stat_root(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
) -> Result<Json<ApiResponse<FileInfo>>, AppError> {
    let info = state.fs_service.stat(&auth.workspace_id, "/").await?;
    Ok(Json(ApiResponse::ok(info)))
}

async fn delete_root(_auth: AuthWorkspace) -> Result<Json<ApiResponse<()>>, AppError> {
    Err(VedaError::InvalidPath("cannot delete root".into()).into())
}

async fn write_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
    body: Result<String, StringRejection>,
) -> Result<Json<ApiResponse<WriteFileResponse>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let body = body.map_err(|_| {
        AppError(VedaError::PayloadTooLarge(format!(
            "max file size is {MAX_BODY_MB}MB"
        )))
    })?;
    let path = format!("/{path}");
    let resp = state
        .fs_service
        .write_file(&auth.workspace_id, &path, &body)
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn append_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
    body: Result<String, StringRejection>,
) -> Result<Json<ApiResponse<WriteFileResponse>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let body = body.map_err(|_| {
        AppError(VedaError::PayloadTooLarge(format!(
            "max file size is {MAX_BODY_MB}MB"
        )))
    })?;
    let path = format!("/{path}");
    let resp = state
        .fs_service
        .append_file(&auth.workspace_id, &path, &body)
        .await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn read_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
    Query(q): Query<FsQuery>,
) -> Result<Response, AppError> {
    let path = format!("/{path}");

    if q.stat.is_some() {
        let info = state.fs_service.stat(&auth.workspace_id, &path).await?;
        return Ok(Json(ApiResponse::ok(info)).into_response());
    }

    if q.list.is_some() {
        let entries = state.fs_service.list_dir(&auth.workspace_id, &path).await?;
        return Ok(Json(ApiResponse::ok(entries)).into_response());
    }

    if let Some(ref lines) = q.lines {
        let parts: Vec<&str> = lines.split(':').collect();
        if parts.len() != 2 {
            return Err(VedaError::InvalidInput("lines format: start:end".into()).into());
        }
        let start: i32 = parts[0]
            .parse()
            .map_err(|_| VedaError::InvalidInput("invalid line number".into()))?;
        let end: i32 = parts[1]
            .parse()
            .map_err(|_| VedaError::InvalidInput("invalid line number".into()))?;
        let content = state
            .fs_service
            .read_file_lines(&auth.workspace_id, &path, start, end)
            .await?;
        return Ok(content.into_response());
    }

    let content = state
        .fs_service
        .read_file(&auth.workspace_id, &path)
        .await?;
    Ok(content.into_response())
}

async fn stat_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
) -> Result<Json<ApiResponse<FileInfo>>, AppError> {
    let path = format!("/{path}");
    let info = state.fs_service.stat(&auth.workspace_id, &path).await?;
    Ok(Json(ApiResponse::ok(info)))
}

async fn delete_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let path = format!("/{path}");
    let _count = state.fs_service.delete(&auth.workspace_id, &path).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct CopyRenameBody {
    from: String,
    to: String,
}

async fn copy_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(body): Json<CopyRenameBody>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    state
        .fs_service
        .copy_file(&auth.workspace_id, &body.from, &body.to)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn rename_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(body): Json<CopyRenameBody>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    state
        .fs_service
        .rename(&auth.workspace_id, &body.from, &body.to)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Deserialize)]
struct MkdirBody {
    path: String,
}

async fn mkdir(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(body): Json<MkdirBody>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    state
        .fs_service
        .mkdir(&auth.workspace_id, &body.path)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

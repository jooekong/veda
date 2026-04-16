use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, head, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use veda_types::api::{DirEntry, FileInfo, WriteFileResponse};
use veda_types::{ApiResponse, VedaError};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/fs/{*path}", put(write_file))
        .route("/v1/fs/{*path}", get(read_file))
        .route("/v1/fs/{*path}", head(stat_file))
        .route("/v1/fs/{*path}", delete(delete_file))
        .route("/v1/fs-copy", post(copy_file))
        .route("/v1/fs-rename", post(rename_file))
        .route("/v1/fs-mkdir", post(mkdir))
}

#[derive(Deserialize, Default)]
struct FsQuery {
    list: Option<String>,
    lines: Option<String>,
}

async fn write_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
    body: String,
) -> Result<Json<ApiResponse<WriteFileResponse>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let path = format!("/{path}");
    let resp = state
        .fs_service
        .write_file(&auth.workspace_id, &path, &body)
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

    if q.list.is_some() {
        let entries = state
            .fs_service
            .list_dir(&auth.workspace_id, &path)
            .await?;
        let dir_entries: Vec<DirEntry> = entries
            .into_iter()
            .map(|d| DirEntry {
                name: d.name.clone(),
                path: d.path.clone(),
                is_dir: d.is_dir,
                size_bytes: None,
                mime_type: None,
            })
            .collect();
        return Ok(Json(ApiResponse::ok(dir_entries)).into_response());
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

    let content = state.fs_service.read_file(&auth.workspace_id, &path).await?;
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
    state.fs_service.delete(&auth.workspace_id, &path).await?;
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

use std::sync::Arc;

use axum::extract::rejection::StringRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
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
    headers: HeaderMap,
    body: Result<String, StringRejection>,
) -> Result<Response, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let body = body.map_err(|_| {
        AppError(VedaError::PayloadTooLarge(format!(
            "max file size is {MAX_BODY_MB}MB"
        )))
    })?;
    let path = format!("/{path}");
    let expected_rev = parse_if_match(&headers);
    let resp = state
        .fs_service
        .write_file(&auth.workspace_id, &path, &body, expected_rev)
        .await?;

    let mut r = Json(ApiResponse::ok(resp.clone())).into_response();
    r.headers_mut().insert(
        header::ETAG,
        header::HeaderValue::from_str(&format!("\"{}\"", resp.revision)).unwrap(),
    );
    Ok(r)
}

fn parse_if_match(h: &HeaderMap) -> Option<i32> {
    let v = h.get(header::IF_MATCH)?.to_str().ok()?;
    let v = v.trim().trim_matches('"');
    v.parse().ok()
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
    headers: HeaderMap,
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

    // Handle Range header for partial reads
    if let Some(range_val) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_val.to_str() {
            if let Some((offset, length)) = parse_range_header(range_str) {
                let (data, total) = state
                    .fs_service
                    .read_file_range(&auth.workspace_id, &path, offset, length)
                    .await?;
                if data.is_empty() {
                    return Ok((
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                    )
                        .into_response());
                }
                let end = offset + data.len() as u64 - 1;
                return Ok((
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_RANGE, format!("bytes {offset}-{end}/{total}")),
                        (header::CONTENT_LENGTH, data.len().to_string()),
                    ],
                    data,
                )
                    .into_response());
            }
        }
    }

    let content = state
        .fs_service
        .read_file(&auth.workspace_id, &path)
        .await?;
    Ok(content.into_response())
}

/// Parse "bytes=start-end" or "bytes=start-" range header.
fn parse_range_header(s: &str) -> Option<(u64, u64)> {
    let s = s.strip_prefix("bytes=")?;
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start: u64 = parts[0].parse().ok()?;
    if parts[1].is_empty() {
        // "bytes=start-" → read from start to end (use a large length)
        Some((start, u64::MAX))
    } else {
        let end: u64 = parts[1].parse().ok()?;
        if end < start {
            return None;
        }
        Some((start, end - start + 1))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_basic() {
        assert_eq!(parse_range_header("bytes=0-99"), Some((0, 100)));
        assert_eq!(parse_range_header("bytes=100-199"), Some((100, 100)));
    }

    #[test]
    fn parse_range_open_end() {
        let (offset, length) = parse_range_header("bytes=500-").unwrap();
        assert_eq!(offset, 500);
        assert_eq!(length, u64::MAX);
    }

    #[test]
    fn parse_range_invalid() {
        assert!(parse_range_header("invalid").is_none());
        assert!(parse_range_header("bytes=abc-def").is_none());
        assert!(parse_range_header("bytes=100-50").is_none());
    }
}

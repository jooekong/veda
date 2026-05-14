use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use veda_types::api::{AbstractResponse, OverviewResponse, SearchApiRequest};
use veda_types::{ApiResponse, DetailLevel, SearchHit};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/search", post(search))
        // Workspace-root summaries: `{*path}` is a greedy wildcard
        // and Axum refuses to match it against an empty segment, so
        // a bare `/v1/abstract` is impossible under the same handler.
        // Pair the wildcard with a no-path variant that hands the
        // service a literal `"/"`. Used by `veda abstract /` and by
        // the FUSE sidecar at the mount root.
        .route("/v1/abstract", get(get_abstract_root))
        .route("/v1/abstract/{*path}", get(get_abstract))
        .route("/v1/overview", get(get_overview_root))
        .route("/v1/overview/{*path}", get(get_overview))
}

async fn get_abstract_root(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
) -> Result<Response, AppError> {
    serve_abstract(state, auth, "/".to_string()).await
}

async fn get_overview_root(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
) -> Result<Response, AppError> {
    serve_overview(state, auth, "/".to_string()).await
}

async fn search(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<SearchApiRequest>,
) -> Result<Json<ApiResponse<Vec<SearchHit>>>, AppError> {
    let mode = req.mode.unwrap_or_default();
    let limit = req.limit.unwrap_or(10).min(100);
    let detail_level = req.detail_level.unwrap_or(DetailLevel::Full);

    let hits = state
        .search_service
        .search(
            &auth.workspace_id,
            &req.query,
            mode,
            limit,
            req.path_prefix.as_deref(),
            detail_level,
        )
        .await?;
    // `SearchHit` serializes to the public shape directly — file_id is
    // marked `#[serde(skip_serializing)]` so it stays server-side.
    Ok(Json(ApiResponse::ok(hits)))
}

async fn get_abstract(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
) -> Result<Response, AppError> {
    serve_abstract(state, auth, format!("/{path}")).await
}

async fn get_overview(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
) -> Result<Response, AppError> {
    serve_overview(state, auth, format!("/{path}")).await
}

async fn serve_abstract(
    state: Arc<AppState>,
    auth: AuthWorkspace,
    path: String,
) -> Result<Response, AppError> {
    let summary = state
        .search_service
        .get_summary(&auth.workspace_id, &path)
        .await?;
    match summary {
        Some(s) => Ok(Json(ApiResponse::ok(AbstractResponse {
            path,
            l0_abstract: s.l0_abstract,
        }))
        .into_response()),
        None => Ok(summary_pending_response(state.summary_enabled)),
    }
}

async fn serve_overview(
    state: Arc<AppState>,
    auth: AuthWorkspace,
    path: String,
) -> Result<Response, AppError> {
    let summary = state
        .search_service
        .get_summary(&auth.workspace_id, &path)
        .await?;
    match summary {
        Some(s) => Ok(Json(ApiResponse::ok(OverviewResponse {
            path,
            l1_overview: s.l1_overview,
        }))
        .into_response()),
        None => Ok(summary_pending_response(state.summary_enabled)),
    }
}

/// Path exists but the summary row is missing. Two distinct cases:
///   a) [llm] is not configured → never will be generated → 501
///   b) [llm] is configured, but L0/L1 are still being produced → 202
/// Without distinguishing these the user can't tell whether to give up
/// or to retry, and we previously had a perpetual-pending bug when
/// [llm] was missing on the alpha server.
fn summary_pending_response(summary_enabled: bool) -> Response {
    if !summary_enabled {
        let body = Json(ApiResponse::<()>::err(
            "summary generation is disabled (server has no [llm] configured)",
        ));
        // RFC 7231 lets clients cache 501 by default. Force no-store so
        // proxies don't pin the "disabled" state — once Joe restarts the
        // server with [llm] configured, clients should see live status.
        let mut resp = (StatusCode::NOT_IMPLEMENTED, body).into_response();
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resp
    } else {
        let body = Json(ApiResponse::<()>::err("summary pending"));
        let mut resp = (StatusCode::ACCEPTED, body).into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        resp
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn search_limit_capped_at_100() {
        let raw: Option<usize> = Some(500);
        let limit = raw.unwrap_or(10).min(100);
        assert_eq!(limit, 100);
    }

    #[test]
    fn search_limit_default_is_10() {
        let raw: Option<usize> = None;
        let limit = raw.unwrap_or(10).min(100);
        assert_eq!(limit, 10);
    }

    #[test]
    fn search_limit_passes_through_when_small() {
        let raw: Option<usize> = Some(50);
        let limit = raw.unwrap_or(10).min(100);
        assert_eq!(limit, 50);
    }
}

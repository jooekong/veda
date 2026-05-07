use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use veda_types::api::{SearchApiRequest, SearchResultItem, SummaryResponse};
use veda_types::{ApiResponse, DetailLevel};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/search", post(search))
        .route("/v1/summary/{*path}", get(get_summary))
}

async fn search(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<SearchApiRequest>,
) -> Result<Json<ApiResponse<Vec<SearchResultItem>>>, AppError> {
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
    let results: Vec<SearchResultItem> = hits.into_iter().map(SearchResultItem::from).collect();

    Ok(Json(ApiResponse::ok(results)))
}

async fn get_summary(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
) -> Result<Response, AppError> {
    let path = format!("/{path}");
    let summary = state
        .search_service
        .get_summary(&auth.workspace_id, &path)
        .await?;

    match summary {
        Some(s) => Ok(Json(ApiResponse::ok(SummaryResponse {
            path,
            l0_abstract: s.l0_abstract,
            l1_overview: s.l1_overview,
        }))
        .into_response()),
        // Path exists but summary row missing. Two distinct cases:
        //   a) [llm] is not configured → it will NEVER be generated → 501
        //   b) [llm] is configured, but L0/L1 still being produced → 202
        // Without distinguishing these the user can't tell whether to give
        // up or to retry, and we previously had a perpetual-pending bug
        // when [llm] was missing on the alpha server.
        None if !state.summary_enabled => {
            let body = Json(ApiResponse::<()>::err(
                "summary generation is disabled (server has no [llm] configured)",
            ));
            // RFC 7231 lets clients cache 501 by default. Force no-store so
            // proxies don't pin the "disabled" state — once Joe restarts the
            // server with [llm] configured, clients should see live status.
            let mut resp = (StatusCode::NOT_IMPLEMENTED, body).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(resp)
        }
        None => {
            let body = Json(ApiResponse::<()>::err("summary pending"));
            let mut resp = (StatusCode::ACCEPTED, body).into_response();
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            Ok(resp)
        }
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

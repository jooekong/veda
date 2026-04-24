use std::sync::Arc;

use axum::extract::{Path, State};
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
    let limit = req.limit.unwrap_or(10);
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
) -> Result<Json<ApiResponse<SummaryResponse>>, AppError> {
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
        }))),
        None => Err(veda_types::VedaError::NotFound("summary not found".into()).into()),
    }
}

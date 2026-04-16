use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use veda_types::api::{SearchApiRequest, SearchResultItem};
use veda_types::ApiResponse;

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/search", post(search))
}

async fn search(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<SearchApiRequest>,
) -> Result<Json<ApiResponse<Vec<SearchResultItem>>>, AppError> {
    let mode = req.mode.unwrap_or_default();
    let limit = req.limit.unwrap_or(10);

    let hits = state
        .search_service
        .search(
            &auth.workspace_id,
            &req.query,
            mode,
            limit,
            req.path_prefix.as_deref(),
        )
        .await?;
    let results: Vec<SearchResultItem> = hits
        .into_iter()
        .map(|h| SearchResultItem {
            path: h.path.unwrap_or_default(),
            chunk_index: h.chunk_index,
            content: h.content,
            score: h.score,
        })
        .collect();

    Ok(Json(ApiResponse::ok(results)))
}

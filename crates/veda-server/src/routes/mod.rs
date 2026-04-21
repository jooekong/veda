pub mod account;
pub mod collection;
pub mod fs;
pub mod search;
pub mod sql;

use crate::state::AppState;
use axum::routing::get;
use axum::{Json, Router};
use std::sync::Arc;
use veda_types::ApiResponse;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .merge(account::routes())
        .merge(fs::routes())
        .merge(search::routes())
        .merge(collection::routes())
        .merge(sql::routes())
        .with_state(state)
}

async fn health() -> Json<ApiResponse<&'static str>> {
    Json(ApiResponse::ok("ok"))
}

pub mod account;
pub mod collection;
pub mod fs;
pub mod search;
pub mod sql;

use std::sync::Arc;
use axum::Router;
use crate::state::AppState;

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(account::routes())
        .merge(fs::routes())
        .merge(search::routes())
        .merge(collection::routes())
        .merge(sql::routes())
        .with_state(state)
}

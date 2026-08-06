//! Per-workspace document heat ranking (`GET /v1/stats/docs`).
//!
//! Read-only over `veda_doc_access_daily`. Metric semantics (what counts,
//! what's exempt) live on `api::DocAccessEntry` and the public reference
//! docs — this handler only windows and orders.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use veda_core::store::DocAccessOrder;
use veda_types::api::DocAccessStatsResponse;
use veda_types::ApiResponse;

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/stats/docs", get(get_doc_stats))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DocStatsQuery {
    days: Option<u32>,
    limit: Option<usize>,
    order_by: Option<String>,
}

/// Shared by the native route and the platform gateway surface so the two
/// cannot drift on clamping or order semantics.
pub(crate) async fn build_doc_stats(
    state: &Arc<AppState>,
    workspace_id: &str,
    q: &DocStatsQuery,
) -> Result<DocAccessStatsResponse, AppError> {
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let order = match q.order_by.as_deref() {
        None | Some("reads") => DocAccessOrder::Reads,
        Some("search_hits") => DocAccessOrder::SearchHits,
        Some(other) => {
            return Err(AppError::from(veda_types::VedaError::InvalidInput(format!(
                "order_by must be 'reads' or 'search_hits', got '{other}'"
            ))))
        }
    };
    let items = state
        .access_recorder
        .query(workspace_id, days, order, limit)
        .await?;
    Ok(DocAccessStatsResponse { days, items })
}

/// AuthWorkspace enforces fs-kind; read-only `wk_` may query — stats are
/// read-only information about the workspace's own documents.
async fn get_doc_stats(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Query(q): Query<DocStatsQuery>,
) -> Result<Json<ApiResponse<DocAccessStatsResponse>>, AppError> {
    let resp = build_doc_stats(&state, &auth.workspace_id, &q).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

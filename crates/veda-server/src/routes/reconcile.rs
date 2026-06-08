//! On-demand MySQL↔Milvus reconcile — replaces the old 6-hourly background loop.
//!
//! `POST /admin/v1/reconcile/{workspace_id}?dry_run=true|false`
//!
//! Auth reuses the ops `metrics_token` (constant-time compared, same as
//! `/v1/metrics`): reconciling an arbitrary workspace is an operator action,
//! not an account action, so it must NOT sit on the `vk_`/`wk_` plane. When
//! `metrics_token` is unset the endpoint 404s (default-deny), same as metrics.
//!
//! `dry_run` defaults to **true** — report drift without mutating. Pass
//! `?dry_run=false` to actually enqueue repairs and delete orphans. Failures
//! (e.g. the Milvus 16384-window list cliff on a very large workspace) surface
//! as a 500 to the caller instead of a silent background skip.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use crate::routes::metrics_auth_ok;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/admin/v1/reconcile/{workspace_id}",
        post(reconcile_workspace),
    )
}

async fn reconcile_workspace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(workspace_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // Ops-token gate FIRST. 404-on-failure (not 401) mirrors /v1/metrics so
    // the endpoint's existence isn't disclosed to unauthenticated callers.
    // The query is a String map (never 4xx-rejects), so a malformed param
    // cannot trip an axum rejection ahead of this gate.
    if !metrics_auth_ok(state.metrics_token.as_deref(), &headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Default true (report only); only an explicit false/0 mutates.
    let dry_run = !matches!(
        params.get("dry_run").map(|s| s.as_str()),
        Some("false") | Some("0")
    );
    match state
        .reconciler
        .reconcile_workspace(&workspace_id, dry_run)
        .await
    {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "workspace_id": report.workspace_id,
                "dry_run": dry_run,
                "chunk_missing": report.chunk_missing,
                "chunk_orphan": report.chunk_orphan,
                "summary_missing": report.summary_missing,
                "summary_orphan": report.summary_orphan,
            })),
        )
            .into_response(),
        Err(e) => {
            // Surface loudly: an operator triggered this and must see the
            // failure (e.g. the Milvus 16384 list cliff on a large workspace)
            // rather than the old background pass's silent per-workspace skip.
            tracing::error!(workspace_id = %workspace_id, err = %e, "on-demand reconcile failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "reconcile_failed",
                    "workspace_id": workspace_id,
                })),
            )
                .into_response()
        }
    }
}

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use veda_types::api::{
    AbstractResponse, LayoutSummaryState, OverviewResponse, SearchApiRequest, WorkspaceLayout,
};
use veda_types::{ApiResponse, DetailLevel, SearchHit, SourceType};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    // NOTE: there is intentionally no bare `/v1/abstract` /
    // `/v1/overview` route for the workspace root. The summary
    // service resolves a row by dentry, and the root path has no
    // dentry — adding the route just produced misleading 404s
    // (caught by the 2026-05-14 adversarial review). When root-level
    // summaries land as a real feature (worker + store), wire them
    // here.
    Router::new()
        .route("/v1/search", post(search))
        .route("/v1/layout", get(get_layout))
        .route("/v1/abstract/{*path}", get(get_abstract))
        .route("/v1/overview/{*path}", get(get_overview))
}

/// Top-level entries a single layout response will carry. Each costs ~100
/// tokens of abstract, so 200 is already about as much as an agent can
/// absorb in one call; past that the honest answer is `truncated: true`
/// and "use search instead".
pub(crate) const LAYOUT_ENTRY_CAP: usize = 200;

/// Shared by the REST route and the MCP `layout` tool so the two surfaces
/// cannot drift — in particular on the rule that `Disabled` relabels the
/// state without stripping abstracts.
pub(crate) async fn build_workspace_layout(
    state: &Arc<AppState>,
    workspace_id: &str,
) -> Result<WorkspaceLayout, veda_types::VedaError> {
    let mut layout = state
        .search_service
        .workspace_layout(workspace_id, LAYOUT_ENTRY_CAP)
        .await?;
    // The service only knows about coverage; whether summaries can ever be
    // produced is server config. This rewrites the state label only —
    // cached abstracts stay in the response, matching what
    // `/v1/abstract/{path}` serves when [llm] is absent.
    if !state.summary_enabled {
        layout.summary_state = LayoutSummaryState::Disabled;
    }
    Ok(layout)
}

async fn get_layout(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
) -> Result<Json<ApiResponse<WorkspaceLayout>>, AppError> {
    Ok(Json(ApiResponse::ok(
        build_workspace_layout(&state, &auth.workspace_id).await?,
    )))
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
        None if never_summarized(&state, &auth.workspace_id, &path).await? => {
            Ok(unsupported_file_type_response())
        }
        None => Ok(summary_pending_response(state.summary_enabled)),
    }
}

/// Whether the file at `path` belongs to a type that will never produce a
/// summary. Images and opaque binaries have no text layer and are not
/// extracted, so no SummarySync is ever enqueued for them — answering 202
/// "retry in a few seconds" strings the caller along forever.
///
/// Only consulted when the summary row is missing, i.e. off the hot path, so
/// the extra dentry+file lookup costs nothing on a normal hit. Everything
/// else (dirs, text, pdf/word) answers false: those either have a summary
/// already or are genuinely still generating one.
async fn never_summarized(
    state: &AppState,
    workspace_id: &str,
    path: &str,
) -> Result<bool, AppError> {
    let Some(dentry) = state.meta_store.get_dentry(workspace_id, path).await? else {
        return Ok(false);
    };
    let Some(file_id) = dentry.file_id.as_deref() else {
        return Ok(false);
    };
    let Some(file) = state.meta_store.get_file(file_id).await? else {
        return Ok(false);
    };
    Ok(is_unsummarizable(file.source_type))
}

/// Pdf/Word are deliberately absent: they acquire a text layer via
/// ExtractSync and then do get L0/L1, so for them "pending" is the honest
/// answer. Keep this in sync with the worker's extract routing — if a type
/// ever becomes extractable, it must drop off this list.
fn is_unsummarizable(source_type: SourceType) -> bool {
    matches!(source_type, SourceType::Image | SourceType::Binary)
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
        None if never_summarized(&state, &auth.workspace_id, &path).await? => {
            Ok(unsupported_file_type_response())
        }
        None => Ok(summary_pending_response(state.summary_enabled)),
    }
}

/// The path exists but its type has no text to summarize — a terminal answer,
/// unlike 202. 415 rather than 404 (the file is there and downloadable) or
/// 400 (nothing wrong with the request); the `UNSUPPORTED_FILE_TYPE` code is
/// the part clients should branch on.
fn unsupported_file_type_response() -> Response {
    let body = Json(ApiResponse::<()>::err(
        "UNSUPPORTED_FILE_TYPE",
        "this file type never gets a summary (images and opaque binaries \
         have no text layer); download it or use search instead",
    ));
    (StatusCode::UNSUPPORTED_MEDIA_TYPE, body).into_response()
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
            "FEATURE_DISABLED",
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
        let body = Json(ApiResponse::<()>::err("PENDING", "summary pending"));
        let mut resp = (StatusCode::ACCEPTED, body).into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Images and opaque binaries are never extracted, so their summary is
    /// not "pending" — it is never coming, and the route must say so.
    #[test]
    fn image_and_binary_never_get_a_summary() {
        assert!(is_unsummarizable(SourceType::Image));
        assert!(is_unsummarizable(SourceType::Binary));
    }

    /// The regression this whole change exists to prevent: pdf/word DO get
    /// summaries now, so they must keep answering 202 "pending" while the
    /// extract → summary handoff is in flight, never a terminal 415.
    #[test]
    fn extractable_and_text_types_stay_pending() {
        assert!(!is_unsummarizable(SourceType::Pdf));
        assert!(!is_unsummarizable(SourceType::Word));
        assert!(!is_unsummarizable(SourceType::Text));
    }

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

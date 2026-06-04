//! app_id-scoped control plane (`/v1/apps/{app_id}/...`).
//!
//! Auth is **externalized to the platform gateway**: these endpoints take NO
//! veda credential. The path `app_id` is the tenant boundary — the gateway is
//! responsible for proving the caller may act as that app_id. veda trusts it
//! and resolves the account behind the app_id, auto-provisioning it on first
//! use (`POST` only).
//!
//! Runs alongside the legacy `vk_` control plane in `account.rs`
//! (`/v1/workspaces`), which stays for console/CLI during the A migration.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;
use veda_types::api::{CreateWorkspaceRequest, PaginatedResponse, PaginationQuery};
use veda_types::{Account, AccountStatus, ApiResponse, VedaError, Workspace};

use crate::error::AppError;
use crate::routes::account::create_workspace_under;
use crate::state::AppState;

const LIST_DEFAULT_LIMIT: u32 = 100;
const LIST_MAX_LIMIT: u32 = 200;

fn clamp_limit(q: &PaginationQuery) -> u32 {
    q.limit.unwrap_or(LIST_DEFAULT_LIMIT).clamp(1, LIST_MAX_LIMIT)
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/apps/{app_id}/workspaces",
            post(create_app_workspace).get(list_app_workspaces),
        )
        .route(
            "/v1/apps/{app_id}/workspaces/{id}",
            delete(delete_app_workspace),
        )
}

fn require_app_id(app_id: &str) -> Result<&str, AppError> {
    let trimmed = app_id.trim();
    if trimmed.is_empty() {
        return Err(VedaError::InvalidInput("app_id must not be empty".into()).into());
    }
    Ok(trimmed)
}

/// Look up the account for `app_id`, treating a **suspended** account as
/// unavailable — mirrors the `vk_` / `wk_` auth paths, which only match active
/// accounts (so ops can lock an app out of the control plane too). Returns
/// `Ok(None)` when the app_id is simply unknown.
async fn lookup_active_account(
    state: &AppState,
    app_id: &str,
) -> Result<Option<Account>, AppError> {
    match state.auth_store.get_account_by_app_id(app_id).await? {
        Some(acc) if acc.status == AccountStatus::Active => Ok(Some(acc)),
        Some(_) => Err(VedaError::Unauthorized("account suspended".into()).into()),
        None => Ok(None),
    }
}

/// Resolve the account for `app_id`, creating it (auto-provisioning the tenant)
/// when absent. Race-safe: a concurrent create that loses the UNIQUE(app_id)
/// race surfaces as `AlreadyExists`, which we resolve by re-reading the winner.
/// Only the account row is created — no `vk_` is minted (A drops account keys).
async fn ensure_account(state: &AppState, app_id: &str) -> Result<Account, AppError> {
    if let Some(acc) = lookup_active_account(state, app_id).await? {
        return Ok(acc);
    }
    let now = Utc::now();
    let account = Account {
        id: Uuid::new_v4().to_string(),
        name: format!("app-{app_id}"),
        email: None,
        password_hash: None,
        app_id: Some(app_id.to_string()),
        status: AccountStatus::Active,
        created_at: now,
        updated_at: now,
    };
    match state.auth_store.create_account(&account).await {
        Ok(()) => Ok(account),
        // Lost the race against a concurrent first-touch of the same app_id;
        // the winner's row now exists — read it back.
        Err(VedaError::AlreadyExists(_)) => lookup_active_account(state, app_id)
            .await?
            .ok_or_else(|| {
                VedaError::Internal("app_id account vanished after duplicate".into()).into()
            }),
        Err(e) => Err(e.into()),
    }
}

/// POST /v1/apps/{app_id}/workspaces — auto-provision the tenant (if new) and
/// create a workspace under it. The path `app_id` is authoritative and stamped
/// onto the workspace; any `app_id` in the body is ignored.
async fn create_app_workspace(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(mut req): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<ApiResponse<Workspace>>), AppError> {
    let app_id = require_app_id(&app_id)?.to_string();
    let account = ensure_account(&state, &app_id).await?;
    req.app_id = Some(app_id);
    let ws = create_workspace_under(&state, account.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(ws))))
}

/// GET /v1/apps/{app_id}/workspaces — list active workspaces under the app.
/// A GET must not have side effects, so an unknown app_id returns an empty page
/// rather than auto-provisioning a tenant.
async fn list_app_workspaces(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(q): Query<PaginationQuery>,
) -> Result<Json<ApiResponse<PaginatedResponse<Workspace>>>, AppError> {
    let app_id = require_app_id(&app_id)?;
    let account = match lookup_active_account(&state, app_id).await? {
        Some(acc) => acc,
        None => {
            return Ok(Json(ApiResponse::ok(PaginatedResponse {
                items: Vec::new(),
                has_more: false,
                next_cursor: None,
            })))
        }
    };
    let limit = clamp_limit(&q);
    let (items, has_more) = state
        .auth_store
        .list_workspaces(&account.id, q.after.as_deref(), limit)
        .await?;
    let next_cursor = if has_more {
        items.last().map(|w| w.id.clone())
    } else {
        None
    };
    Ok(Json(ApiResponse::ok(PaginatedResponse {
        items,
        has_more,
        next_cursor,
    })))
}

/// DELETE /v1/apps/{app_id}/workspaces/{id} — soft-delete a workspace under the
/// app. Confirms the workspace belongs to this app's account first; a missing
/// or cross-tenant id both return the same `NOT_FOUND` so a probe can't learn
/// whether another app's workspace id exists.
async fn delete_app_workspace(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let app_id = require_app_id(&app_id)?;
    let account = lookup_active_account(&state, app_id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {ws_id}")))?;
    let ws = state
        .auth_store
        .get_workspace(&ws_id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {ws_id}")))?;
    if ws.account_id != account.id {
        return Err(VedaError::NotFound(format!("workspace {ws_id}")).into());
    }
    state.auth_store.delete_workspace(&ws_id).await?;
    Ok(Json(ApiResponse::ok(())))
}

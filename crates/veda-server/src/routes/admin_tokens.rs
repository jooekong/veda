//! Stage 4.4 — admin endpoints for service token management.
//!
//! `POST /admin/v1/tokens` mints a new `vk_` token scoped to the caller's
//! account. v0 has no real admin gate: any logged-in account holder can
//! issue service tokens for **their own** account. Real admin RBAC is v1.
//!
//! `POST /admin/v1/tokens/{id}/disable` verifies the key belongs to the
//! caller's account before revoking — `auth_store.revoke_api_key` has no
//! ownership check, so this layer is the only safeguard against
//! cross-account revoke.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_types::api::{CreateTokenRequest, CreateTokenResponse};
use veda_types::{ApiKeyRecord, ApiResponse, KeyStatus, VedaError};

use crate::auth::AuthAccount;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/v1/tokens", post(create_token))
        .route("/admin/v1/tokens/{id}/disable", post(disable_token))
}

async fn create_token(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<ApiResponse<CreateTokenResponse>>), AppError> {
    if req.app_id.trim().is_empty() {
        return Err(VedaError::InvalidInput("app_id must not be empty".into()).into());
    }
    if req.name.trim().is_empty() {
        return Err(VedaError::InvalidInput("name must not be empty".into()).into());
    }

    // Validate every allowed_workspaces entry actually belongs to the
    // caller's account — without this, callers could mint tokens scoped to
    // someone else's workspace and use the token to bypass the regular
    // load_db_workspace ownership check.
    if let Some(ws_ids) = &req.allowed_workspaces {
        for ws_id in ws_ids {
            let ws = state
                .auth_store
                .get_workspace(ws_id)
                .await?
                .ok_or_else(|| VedaError::NotFound(format!("workspace {ws_id}")))?;
            if ws.account_id != auth.account_id {
                return Err(VedaError::PermissionDenied.into());
            }
        }
    }

    let id = Uuid::new_v4().to_string();
    let raw = format!("vk_{}", Uuid::new_v4().simple());
    let key_hash = sha256_hex(raw.as_bytes());
    let now = Utc::now();
    // If the caller supplied `expires_at` but the ms epoch is out of
    // chrono's representable range, refuse with 400 instead of silently
    // dropping to None (which would mint a never-expiring token — the
    // opposite of the caller's intent and a security footgun).
    let expires_at = match req.expires_at {
        Some(ms) => Some(
            chrono::DateTime::<Utc>::from_timestamp_millis(ms).ok_or_else(|| {
                VedaError::InvalidInput(format!(
                    "expires_at {ms} is not a valid epoch ms timestamp"
                ))
            })?,
        ),
        None => None,
    };
    let record = ApiKeyRecord {
        id: id.clone(),
        account_id: auth.account_id.clone(),
        name: req.name,
        key_hash,
        status: KeyStatus::Active,
        app_id: Some(req.app_id),
        allowed_workspaces: req.allowed_workspaces,
        expires_at,
        created_at: now,
    };
    state.auth_store.create_api_key(&record).await?;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::ok(CreateTokenResponse { id, token: raw })),
    ))
}

async fn disable_token(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    // Ownership check FIRST. revoke_api_key is unconditional by design
    // (no ownership param) — without this guard, any account could revoke
    // any other account's tokens just by knowing the id.
    let key = state
        .auth_store
        .get_api_key_by_id(&id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("token {id}")))?;
    if key.account_id != auth.account_id {
        // Don't disclose existence of cross-account tokens — return the
        // same NotFound the missing-id case would.
        return Err(VedaError::NotFound(format!("token {id}")).into());
    }
    state.auth_store.revoke_api_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

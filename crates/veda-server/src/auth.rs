use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::error;
use veda_types::ApiResponse;

use crate::state::AppState;

const JWT_ISSUER: &str = "veda";

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub iss: String,
    pub workspace_id: String,
    pub account_id: String,
    pub exp: i64,
}

pub fn create_jwt(
    secret: &str,
    workspace_id: &str,
    account_id: &str,
    ttl_hours: i64,
) -> anyhow::Result<(String, chrono::DateTime<Utc>)> {
    let expires_at = Utc::now() + Duration::hours(ttl_hours);
    let claims = JwtClaims {
        sub: workspace_id.to_string(),
        iss: JWT_ISSUER.to_string(),
        workspace_id: workspace_id.to_string(),
        account_id: account_id.to_string(),
        exp: expires_at.timestamp(),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok((token, expires_at))
}

pub fn verify_jwt(secret: &str, token: &str) -> Option<JwtClaims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[JWT_ISSUER]);
    decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()
    .map(|d| d.claims)
}

pub fn validate_jwt_secret(secret: &str) -> anyhow::Result<()> {
    if secret.len() < 32 {
        anyhow::bail!("jwt_secret must be at least 32 bytes");
    }
    Ok(())
}

pub struct AuthAccount {
    pub account_id: String,
    /// Token's `app_id` governance label. `None` for legacy account-owner
    /// keys; `Some` for service tokens issued for company apps.
    pub app_id: Option<String>,
    /// Token's workspace scope. `None` = unrestricted (token can access
    /// any workspace under its account). `Some(list)` = token can only
    /// access workspaces whose `id` is in the list.
    pub allowed_workspaces: Option<Vec<String>>,
}

impl AuthAccount {
    /// Returns Err if `allowed_workspaces` is `Some` and `workspace_id`
    /// is not in the list. Returns Ok if unrestricted or in scope.
    /// Stage 4 vectors handlers call this before any operation against
    /// a workspace_id pulled from the request body.
    pub fn check_workspace_allowed(
        &self,
        workspace_id: &str,
    ) -> Result<(), crate::error::AppError> {
        if let Some(allowed) = &self.allowed_workspaces {
            if !allowed.iter().any(|w| w == workspace_id) {
                return Err(veda_types::VedaError::PermissionDenied.into());
            }
        }
        Ok(())
    }
}

impl FromRequestParts<Arc<AppState>> for AuthAccount {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let state = state.clone();
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        async move {
            let token = auth_header
                .as_deref()
                .and_then(|s| s.strip_prefix("Bearer "))
                .ok_or_else(auth_err)?;

            let key_hash = veda_core::checksum::sha256_hex(token.as_bytes());
            // get_api_key_by_hash filters out expired tokens at the SQL
            // layer (WHERE expires_at IS NULL OR expires_at > NOW()), so an
            // expired token appears as "not found" → 401, matching the spec
            // (don't leak the existence of expired tokens to attackers).
            let key = state
                .auth_store
                .get_api_key_by_hash(&key_hash)
                .await
                .map_err(|e| {
                    error!(err = %e, "auth store error");
                    internal_err()
                })?
                .ok_or_else(auth_err)?;

            Ok(AuthAccount {
                account_id: key.account_id,
                app_id: key.app_id,
                allowed_workspaces: key.allowed_workspaces,
            })
        }
    }
}

pub struct AuthWorkspace {
    pub workspace_id: String,
    pub _account_id: String,
    pub read_only: bool,
}

impl AuthWorkspace {
    pub fn require_write(&self) -> Result<(), crate::error::AppError> {
        if self.read_only {
            return Err(veda_types::VedaError::PermissionDenied.into());
        }
        Ok(())
    }
}

impl AuthAccount {
    pub async fn load_owned_workspace(
        &self,
        state: &AppState,
        ws_id: &str,
    ) -> Result<veda_types::Workspace, crate::error::AppError> {
        let ws = state
            .auth_store
            .get_workspace(ws_id)
            .await?
            .ok_or_else(|| veda_types::VedaError::NotFound("workspace".into()))?;
        if ws.account_id != self.account_id {
            return Err(veda_types::VedaError::PermissionDenied.into());
        }
        Ok(ws)
    }

    /// Resolve and authorize a db-kind workspace for a vector API call.
    /// Bundles all the checks Stage 4 handlers must run before any vector
    /// op: workspace exists + active, account ownership, kind == Db, and
    /// the token's allowed_workspaces scope. Stage 4 handlers call this
    /// instead of stitching individual checks together — forgetting any
    /// one is a silent security gap (Codex Q2 in Stage 1.7 review).
    pub async fn load_db_workspace(
        &self,
        state: &AppState,
        ws_id: &str,
    ) -> Result<veda_types::Workspace, crate::error::AppError> {
        let ws = self.load_owned_workspace(state, ws_id).await?;
        if ws.status != veda_types::WorkspaceStatus::Active {
            return Err(veda_types::VedaError::NotFound("workspace".into()).into());
        }
        if ws.kind != veda_types::WorkspaceKind::Db {
            return Err(veda_types::VedaError::WorkspaceKindMismatch.into());
        }
        self.check_workspace_allowed(&ws.id)?;
        Ok(ws)
    }
}

impl FromRequestParts<Arc<AppState>> for AuthWorkspace {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let state = state.clone();
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        async move {
            let token = auth_header
                .as_deref()
                .and_then(|s| s.strip_prefix("Bearer "))
                .ok_or_else(auth_err)?;

            let (workspace_id, mut account_id, read_only) =
                if let Some(claims) = verify_jwt(&state.jwt_secret, token) {
                    // JWT carries no DB-validated state. Verify the bearer's
                    // account is still active — the workspace_key path
                    // enforces this via SQL JOIN, but JWT must check explicitly.
                    let account = state
                        .auth_store
                        .get_account(&claims.account_id)
                        .await
                        .map_err(|e| {
                            error!(err = %e, "auth store error");
                            internal_err()
                        })?
                        .ok_or_else(auth_err)?;
                    if account.status != veda_types::AccountStatus::Active {
                        return Err(auth_err());
                    }
                    (claims.workspace_id, claims.account_id, false)
                } else {
                    let key_hash = veda_core::checksum::sha256_hex(token.as_bytes());
                    let wk = state
                        .auth_store
                        .get_workspace_key_by_hash(&key_hash)
                        .await
                        .map_err(|e| {
                            error!(err = %e, "auth store error");
                            internal_err()
                        })?
                        .ok_or_else(auth_err)?;
                    let read_only = wk.permission == veda_types::KeyPermission::Read;
                    (wk.workspace_id, String::new(), read_only)
                };

            let ws = state
                .auth_store
                .get_workspace(&workspace_id)
                .await
                .map_err(|e| {
                    error!(err = %e, "auth store error");
                    internal_err()
                })?
                .ok_or_else(auth_err)?;
            if ws.status != veda_types::WorkspaceStatus::Active {
                return Err(auth_err());
            }
            if ws.kind != veda_types::WorkspaceKind::Fs {
                return Err(kind_mismatch_err());
            }

            if account_id.is_empty() {
                account_id = ws.account_id.clone();
            }

            Ok(AuthWorkspace {
                workspace_id,
                _account_id: account_id,
                read_only,
            })
        }
    }
}

fn auth_err() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiResponse::<()>::err("UNAUTHORIZED", "unauthorized")),
    )
        .into_response()
}

fn internal_err() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiResponse::<()>::err("INTERNAL", "internal server error")),
    )
        .into_response()
}

fn kind_mismatch_err() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiResponse::<()>::err(
            "WORKSPACE_KIND_MISMATCH",
            "workspace kind does not match this API path",
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(allowed: Option<Vec<&str>>) -> AuthAccount {
        AuthAccount {
            account_id: "acct".into(),
            app_id: None,
            allowed_workspaces: allowed.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn check_workspace_allowed_unrestricted() {
        assert!(auth(None).check_workspace_allowed("any").is_ok());
    }

    #[test]
    fn check_workspace_allowed_in_scope() {
        let a = auth(Some(vec!["ws-a", "ws-b"]));
        assert!(a.check_workspace_allowed("ws-a").is_ok());
        assert!(a.check_workspace_allowed("ws-b").is_ok());
    }

    #[test]
    fn check_workspace_allowed_out_of_scope() {
        let a = auth(Some(vec!["ws-a"]));
        assert!(a.check_workspace_allowed("ws-c").is_err());
    }

    #[test]
    fn check_workspace_allowed_empty_list_denies_all() {
        // An empty allow-list (set explicitly to vec![]) denies everything —
        // distinct from None (unrestricted).
        let a = auth(Some(vec![]));
        assert!(a.check_workspace_allowed("ws-a").is_err());
    }
}

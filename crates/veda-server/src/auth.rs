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

            let (workspace_id, account_id, read_only) =
                if let Some(claims) = verify_jwt(&state.jwt_secret, token) {
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
        Json(ApiResponse::<()>::err("unauthorized")),
    )
        .into_response()
}

fn internal_err() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiResponse::<()>::err("internal server error")),
    )
        .into_response()
}

use std::sync::Arc;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{Path, State};
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::Utc;
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_types::api::{
    CreateAccountRequest, CreateAccountResponse, CreateWorkspaceRequest, LoginRequest,
    LoginResponse, WorkspaceTokenResponse,
};
use veda_types::{
    Account, AccountStatus, ApiKeyRecord, ApiResponse, KeyPermission, KeyStatus, VedaError,
    Workspace, WorkspaceKey, WorkspaceStatus,
};

use crate::auth::{create_jwt, AuthAccount};
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/accounts", post(create_account))
        .route("/v1/accounts/login", post(login))
        .route(
            "/v1/workspaces",
            post(create_workspace).get(list_workspaces),
        )
        .route("/v1/workspaces/{id}", delete(delete_workspace))
        .route("/v1/workspaces/{id}/keys", post(create_workspace_key))
        .route("/v1/workspaces/{id}/token", post(create_token))
}

async fn create_account(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<Json<ApiResponse<CreateAccountResponse>>, AppError> {
    let existing = state.auth_store.get_account_by_email(&req.email).await?;
    if existing.is_some() {
        return Err(VedaError::AlreadyExists("email already registered".into()).into());
    }

    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| VedaError::Internal(e.to_string()))?
        .to_string();

    let account_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let account = Account {
        id: account_id.clone(),
        name: req.name,
        email: Some(req.email),
        password_hash: Some(password_hash),
        status: AccountStatus::Active,
        created_at: now,
        updated_at: now,
    };
    state.auth_store.create_account(&account).await?;

    let raw_key = format!("vk_{}", Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = sha256_hex(raw_key.as_bytes());
    let api_key = ApiKeyRecord {
        id: Uuid::new_v4().to_string(),
        account_id: account_id.clone(),
        name: "default".into(),
        key_hash,
        status: KeyStatus::Active,
        created_at: now,
    };
    state.auth_store.create_api_key(&api_key).await?;

    Ok(Json(ApiResponse::ok(CreateAccountResponse {
        account_id,
        api_key: raw_key,
    })))
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<ApiResponse<LoginResponse>>, AppError> {
    let account = state
        .auth_store
        .get_account_by_email(&req.email)
        .await?
        .ok_or_else(|| VedaError::Unauthorized("invalid email or password".into()))?;

    let hash_str = account
        .password_hash
        .as_deref()
        .ok_or_else(|| VedaError::Unauthorized("invalid email or password".into()))?;
    let parsed = PasswordHash::new(hash_str)
        .map_err(|_| VedaError::Unauthorized("invalid email or password".into()))?;
    Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed)
        .map_err(|_| VedaError::Unauthorized("invalid email or password".into()))?;

    // Revoke previous login keys to prevent unbounded accumulation.
    let old_keys = state.auth_store.list_api_keys(&account.id).await?;
    for k in &old_keys {
        if k.name == "login" && k.status == KeyStatus::Active {
            state.auth_store.revoke_api_key(&k.id).await?;
        }
    }

    let raw_key = format!("vk_{}", Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = sha256_hex(raw_key.as_bytes());
    let now = Utc::now();
    let api_key = ApiKeyRecord {
        id: Uuid::new_v4().to_string(),
        account_id: account.id.clone(),
        name: "login".into(),
        key_hash,
        status: KeyStatus::Active,
        created_at: now,
    };
    state.auth_store.create_api_key(&api_key).await?;

    Ok(Json(ApiResponse::ok(LoginResponse {
        account_id: account.id,
        api_key: raw_key,
    })))
}

async fn create_workspace(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<Json<ApiResponse<Workspace>>, AppError> {
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let ws = Workspace {
        id: ws_id,
        account_id: auth.account_id,
        name: req.name,
        status: WorkspaceStatus::Active,
        created_at: now,
        updated_at: now,
    };
    state.auth_store.create_workspace(&ws).await?;
    Ok(Json(ApiResponse::ok(ws)))
}

async fn list_workspaces(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
) -> Result<Json<ApiResponse<Vec<Workspace>>>, AppError> {
    let list = state.auth_store.list_workspaces(&auth.account_id).await?;
    Ok(Json(ApiResponse::ok(list)))
}

async fn delete_workspace(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    let _ws = auth.load_owned_workspace(&state, &id).await?;
    state.auth_store.delete_workspace(&id).await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn create_workspace_key(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(ws_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    let _ws = auth.load_owned_workspace(&state, &ws_id).await?;

    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let perm = body
        .get("permission")
        .and_then(|v| v.as_str())
        .unwrap_or("readwrite");
    let permission = match perm {
        "read" => KeyPermission::Read,
        "readwrite" => KeyPermission::ReadWrite,
        other => {
            return Err(VedaError::InvalidInput(format!(
                "unknown permission '{other}', expected 'read' or 'readwrite'"
            ))
            .into())
        }
    };

    let raw_key = format!("wk_{}", Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = sha256_hex(raw_key.as_bytes());
    let now = Utc::now();
    let wk = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws_id,
        name,
        key_hash,
        permission,
        status: KeyStatus::Active,
        created_at: now,
    };
    state.auth_store.create_workspace_key(&wk).await?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "key": raw_key,
        "permission": perm,
    }))))
}

async fn create_token(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(ws_id): Path<String>,
) -> Result<Json<ApiResponse<WorkspaceTokenResponse>>, AppError> {
    let _ws = auth.load_owned_workspace(&state, &ws_id).await?;
    let (token, expires_at) = create_jwt(&state.jwt_secret, &ws_id, &auth.account_id, 24)
        .map_err(|e| VedaError::Internal(e.to_string()))?;
    Ok(Json(ApiResponse::ok(WorkspaceTokenResponse {
        token,
        expires_at,
    })))
}

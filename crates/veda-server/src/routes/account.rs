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
    AnonymousOnboardResponse, ClaimAccountRequest, ClaimAccountResponse, CreateAccountRequest,
    CreateAccountResponse, CreateWorkspaceRequest, LoginRequest, LoginResponse,
    WorkspaceTokenResponse,
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
        .route("/v1/accounts/anonymous", post(create_anonymous_account))
        .route("/v1/accounts/claim", post(claim_account))
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

    // Suspended accounts cannot mint new keys. Use the same generic error so
    // the response does not disclose whether the account exists or is locked.
    if account.status != AccountStatus::Active {
        return Err(VedaError::Unauthorized("invalid email or password".into()).into());
    }

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

/// Zero-input onboarding. Mints an anonymous account, a default
/// workspace, and both account- and workspace-scoped keys in one
/// round-trip so a fresh CLI is fully usable after a single POST.
///
/// `name` is auto-generated `anon-{8hex}`; `email` and `password_hash`
/// stay NULL until `claim` is called. The unique index on `email`
/// allows multiple NULL rows, so anonymous accounts don't collide.
async fn create_anonymous_account(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ApiResponse<AnonymousOnboardResponse>>, AppError> {
    let now = Utc::now();
    let account_id = Uuid::new_v4().to_string();
    let name = format!("anon-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let account = Account {
        id: account_id.clone(),
        name,
        email: None,
        password_hash: None,
        status: AccountStatus::Active,
        created_at: now,
        updated_at: now,
    };
    let raw_api_key = format!("vk_{}", Uuid::new_v4().simple());
    let api_key_hash = sha256_hex(raw_api_key.as_bytes());
    let api_key_record = ApiKeyRecord {
        id: Uuid::new_v4().to_string(),
        account_id: account_id.clone(),
        name: "anonymous".into(),
        key_hash: api_key_hash,
        status: KeyStatus::Active,
        created_at: now,
    };

    let workspace_id = Uuid::new_v4().to_string();
    let workspace = Workspace {
        id: workspace_id.clone(),
        account_id: account_id.clone(),
        name: "default".into(),
        status: WorkspaceStatus::Active,
        created_at: now,
        updated_at: now,
    };

    let raw_ws_key = format!("wk_{}", Uuid::new_v4().simple());
    let ws_key_hash = sha256_hex(raw_ws_key.as_bytes());
    let ws_key = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: workspace_id.clone(),
        name: "cli".into(),
        key_hash: ws_key_hash,
        permission: KeyPermission::ReadWrite,
        status: KeyStatus::Active,
        created_at: now,
    };

    // One transaction across the 4 inserts: account + vk_ + workspace
    // + wk_. If any of them fail (UNIQUE collision, pool drop, …)
    // none persist, so we never leave an orphan account.
    state
        .auth_store
        .create_anonymous_bundle(&account, &api_key_record, &workspace, &ws_key)
        .await?;

    Ok(Json(ApiResponse::ok(AnonymousOnboardResponse {
        account_id,
        api_key: raw_api_key,
        workspace_id,
        workspace_key: raw_ws_key,
    })))
}

/// Upgrade an anonymous account to a named one. Requires the existing
/// anonymous `vk_` for auth; keeps the same `api_key` valid after the
/// upgrade so the CLI doesn't have to re-mint. Refuses if the account
/// is already named or if the email is taken — keeps `email IS NULL`
/// as the canonical "anonymous" marker.
/// Pure-input validation for `ClaimAccountRequest`. Reject empty
/// strings before reaching the DB so we don't store rows like
/// `email = ""` that the IS NULL guard can't distinguish from real
/// anonymous accounts. Extracted so tests can pin the rules without
/// spinning up axum + state.
fn validate_claim_input(req: &ClaimAccountRequest) -> std::result::Result<(), VedaError> {
    if req.email.trim().is_empty() {
        return Err(VedaError::InvalidInput("email cannot be empty".into()));
    }
    if req.password.is_empty() {
        return Err(VedaError::InvalidInput("password cannot be empty".into()));
    }
    if let Some(n) = req.name.as_deref() {
        if n.trim().is_empty() {
            return Err(VedaError::InvalidInput(
                "name cannot be empty (omit the field to keep current name)".into(),
            ));
        }
    }
    Ok(())
}

async fn claim_account(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<ClaimAccountRequest>,
) -> Result<Json<ApiResponse<ClaimAccountResponse>>, AppError> {
    validate_claim_input(&req)?;

    let account = state
        .auth_store
        .get_account(&auth.account_id)
        .await?
        .ok_or_else(|| VedaError::NotFound("account".into()))?;
    if account.email.is_some() {
        return Err(VedaError::InvalidInput(
            "account is already claimed (has an email)".into(),
        )
        .into());
    }
    // Pre-check email collision for a friendly 409, but the store's
    // `WHERE email IS NULL` guard + 1062 translation also covers the
    // race where two clients claim the same email concurrently.
    if state
        .auth_store
        .get_account_by_email(&req.email)
        .await?
        .is_some()
    {
        return Err(VedaError::AlreadyExists("email already registered".into()).into());
    }

    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| VedaError::Internal(e.to_string()))?
        .to_string();

    state
        .auth_store
        .claim_account(&account.id, &req.email, &password_hash, req.name.as_deref())
        .await?;

    Ok(Json(ApiResponse::ok(ClaimAccountResponse {
        account_id: account.id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(email: &str, password: &str, name: Option<&str>) -> ClaimAccountRequest {
        ClaimAccountRequest {
            email: email.into(),
            password: password.into(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn validate_claim_input_accepts_normal_request() {
        assert!(validate_claim_input(&req("a@b.com", "hunter2", None)).is_ok());
        assert!(
            validate_claim_input(&req("a@b.com", "hunter2", Some("Joe"))).is_ok()
        );
    }

    #[test]
    fn validate_claim_input_rejects_empty_email() {
        // Empty string, after trim, must fail. Otherwise `email = ""`
        // ends up stored as a real row that the IS NULL anonymous
        // marker can't catch.
        for empty in ["", "   ", "\t"] {
            let err = validate_claim_input(&req(empty, "hunter2", None)).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("email"), "got: {msg}");
        }
    }

    #[test]
    fn validate_claim_input_rejects_empty_password() {
        let err = validate_claim_input(&req("a@b.com", "", None)).unwrap_err();
        assert!(err.to_string().contains("password"), "got: {err}");
    }

    #[test]
    fn validate_claim_input_rejects_explicit_empty_name() {
        // Some("") and Some("   ") are user intent to rename, not
        // "keep current"; reject so we don't end up with a blank name.
        // None means "keep current", which is fine.
        for blank in ["", "   "] {
            let err = validate_claim_input(&req("a@b.com", "hunter2", Some(blank)))
                .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("name"), "got: {msg}");
        }
    }

    #[test]
    fn claim_request_deserializes_with_optional_name() {
        let with: ClaimAccountRequest = serde_json::from_str(
            r#"{"email":"a@b.com","password":"x","name":"Joe"}"#,
        )
        .unwrap();
        assert_eq!(with.name.as_deref(), Some("Joe"));

        let without: ClaimAccountRequest =
            serde_json::from_str(r#"{"email":"a@b.com","password":"x"}"#).unwrap();
        assert!(without.name.is_none());
    }
}

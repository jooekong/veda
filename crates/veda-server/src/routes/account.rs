use std::sync::Arc;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::Utc;
use tracing::warn;
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_types::api::{
    AnonymousOnboardResponse, ClaimAccountRequest, ClaimAccountResponse, CreateAccountRequest,
    CreateAccountResponse, CreateWorkspaceRequest, LoginRequest, LoginResponse, PaginatedResponse,
    PaginationQuery,
};
use veda_types::{
    Account, AccountStatus, ApiKeyRecord, ApiResponse, Dataset, DatasetStatus, KeyPermission,
    KeyStatus, VedaError, Workspace, WorkspaceKey, WorkspaceKind, WorkspaceStatus,
};

use crate::auth::AuthAccount;
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
        .route(
            "/v1/workspaces/{id}/keys",
            post(create_workspace_key).get(list_workspace_keys),
        )
        .route(
            "/v1/workspaces/{id}/keys/{key_id}",
            delete(delete_workspace_key),
        )
}

async fn create_account(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<Json<ApiResponse<CreateAccountResponse>>, AppError> {
    let now = Utc::now();
    let account_id = Uuid::new_v4().to_string();

    // Two creation modes:
    //   - app_id mode (platform): app_id set, no email/password. The vk_ is
    //     returned once here and the platform keeps it (no email login, no v0
    //     re-issue path). app_id uniqueness is enforced by the DB (→ 409).
    //   - email mode (console/CLI): email + password, no app_id.
    let (email, password_hash, app_id) = match (&req.app_id, &req.email, &req.password) {
        // app_id mode (platform): app_id only. Reject mixed input rather than
        // silently dropping email/password into a passwordless account.
        (Some(app_id), None, None) => {
            let app_id = app_id.trim();
            if app_id.is_empty() {
                return Err(VedaError::InvalidInput("app_id must not be empty".into()).into());
            }
            (None, None, Some(app_id.to_string()))
        }
        (Some(_), _, _) => {
            return Err(
                VedaError::InvalidInput("app_id mode must omit email/password".into()).into(),
            )
        }
        (None, Some(email), Some(password)) => {
            if state.auth_store.get_account_by_email(email).await?.is_some() {
                return Err(VedaError::AlreadyExists("email already registered".into()).into());
            }
            let salt = SaltString::generate(&mut OsRng);
            let hash = Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map_err(|e| VedaError::Internal(e.to_string()))?
                .to_string();
            (Some(email.clone()), Some(hash), None)
        }
        _ => {
            return Err(VedaError::InvalidInput(
                "provide either app_id (platform) or email + password".into(),
            )
            .into())
        }
    };

    let account = Account {
        id: account_id.clone(),
        name: req.name,
        email,
        password_hash,
        app_id: app_id.clone(),
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
        // Stamp app_id on the token too (governance label for ops traceability).
        app_id: app_id.clone(),
        allowed_workspaces: None,
        expires_at: None,
        created_at: now,
    };
    state.auth_store.create_api_key(&api_key).await?;

    Ok(Json(ApiResponse::ok(CreateAccountResponse {
        account_id,
        api_key: raw_key,
        app_id,
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
        app_id: None,
        allowed_workspaces: None,
        expires_at: None,
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
        app_id: None,
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
        app_id: None,
        allowed_workspaces: None,
        expires_at: None,
        created_at: now,
    };

    let workspace_id = Uuid::new_v4().to_string();
    let workspace = Workspace {
        id: workspace_id.clone(),
        account_id: account_id.clone(),
        name: "default".into(),
        status: WorkspaceStatus::Active,
        kind: WorkspaceKind::Fs,
        app_id: None,
        description: None,
        created_at: now,
        updated_at: now,
    };

    let raw_ws_key = format!("wk_{}", Uuid::new_v4().simple());
    let ws_key_hash = sha256_hex(raw_ws_key.as_bytes());
    let ws_key = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: workspace_id.clone(),
        account_id: account_id.clone(),
        name: "cli".into(),
        key_hash: ws_key_hash,
        permission: KeyPermission::ReadWrite,
        status: KeyStatus::Active,
        kind: WorkspaceKind::Fs,
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
    // app_id accounts belong to the platform — passwordless by design. Claim
    // must not convert one into an email/password login (that would let a
    // leaked vk_ hijack the account and break the platform's control).
    if account.app_id.is_some() {
        return Err(VedaError::InvalidInput("app_id accounts cannot be claimed".into()).into());
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
    let ws = create_workspace_under(&state, auth.account_id, req).await?;
    Ok(Json(ApiResponse::ok(ws)))
}

/// Build a workspace (fs or db) under an already-resolved `account_id`. Shared
/// by the `vk_` control plane (`POST /v1/workspaces`, account from the bearer)
/// and the workspace-scoped control plane (`POST /v1/workspace/{workspace}/projects`,
/// account auto-provisioned from the path). For `kind=db`, commits the workspace +
/// bootstrap `default` dataset in one tx, then provisions the Milvus collection
/// with rollback on failure. `req.app_id` is the workspace's governance label;
/// the workspace-plane handler sets it to the path workspace code.
pub(crate) async fn create_workspace_under(
    state: &AppState,
    account_id: String,
    req: CreateWorkspaceRequest,
) -> Result<Workspace, AppError> {
    let now = Utc::now();
    let ws = Workspace {
        id: Uuid::new_v4().to_string(),
        account_id,
        name: req.name,
        status: WorkspaceStatus::Active,
        kind: req.kind,
        app_id: req.app_id,
        description: req.description,
        created_at: now,
        updated_at: now,
    };
    if ws.kind == WorkspaceKind::Db {
        // workspace + bootstrap dataset commit together in one tx (no
        // orphan-workspace window), then provision the Milvus collection
        // with rollback on failure.
        let default_dataset = Dataset {
            id: Uuid::new_v4().to_string(),
            workspace_id: ws.id.clone(),
            name: veda_types::validate::DEFAULT_DATASET.to_string(),
            status: DatasetStatus::Active,
            description: None,
            created_at: ws.created_at,
            updated_at: ws.updated_at,
        };
        state
            .auth_store
            .create_db_workspace(&ws, &default_dataset)
            .await?;
        provision_db_collection(state, &ws).await?;
    } else {
        state.auth_store.create_workspace(&ws).await?;
    }

    Ok(ws)
}

/// Create the Milvus collection for an already-persisted db workspace (its
/// workspace + default dataset rows were committed together by
/// `create_db_workspace`). On failure, roll back the DB metadata FIRST, then
/// drop the partial collection. Order matters: if we crash mid-rollback,
/// dropping the control-plane rows first means the user sees a clean "no such
/// workspace" rather than a zombie workspace they can list but can't use
/// (collection gone); the leftover orphan collection is pure storage waste
/// that the archived-resource GC (todo H1) reclaims. All steps are idempotent
/// (drop swallows not-exists), so partial rollback failures don't compound.
async fn provision_db_collection(state: &AppState, ws: &Workspace) -> Result<(), AppError> {
    if let Err(e) = state
        .vector_workspace_store
        .create_vector_collection(&ws.id, state.embedding_dim)
        .await
    {
        if let Err(rb) = state
            .auth_store
            .hard_delete_datasets_for_workspace(&ws.id)
            .await
        {
            warn!(
                workspace_id = %ws.id,
                provision_err = %e,
                rollback_err = %rb,
                "rollback hard_delete_datasets failed",
            );
        }
        if let Err(rb) = state.auth_store.hard_delete_workspace(&ws.id).await {
            warn!(
                workspace_id = %ws.id,
                provision_err = %e,
                rollback_err = %rb,
                "rollback hard_delete_workspace failed",
            );
        }
        let collection_name = veda_store::vector_collection_name(&ws.id);
        if let Err(rb) = state.vector_workspace_store.drop_collection(&collection_name).await {
            warn!(
                workspace_id = %ws.id,
                collection_name = %collection_name,
                provision_err = %e,
                rollback_err = %rb,
                "rollback drop_collection failed after milvus create error; \
                 orphan collection may remain (reclaimed by archived-resource GC)",
            );
        }
        return Err(e.into());
    }

    Ok(())
}

/// Default page size when caller omits `?limit=`. 100 fits in a single
/// TCP frame for typical workspace metadata and matches Pinecone / Stripe
/// list defaults.
const LIST_DEFAULT_LIMIT: u32 = 100;
/// Hard cap on `?limit=`. Higher values are clamped down silently to
/// protect the server from accidental DoS-by-paging. Server-side
/// fetch_all of >200 rows on a single page also starts to push response
/// sizes past axum's default tower-http body-limit.
const LIST_MAX_LIMIT: u32 = 200;

fn clamp_limit(q: &PaginationQuery) -> u32 {
    q.limit
        .unwrap_or(LIST_DEFAULT_LIMIT)
        .clamp(1, LIST_MAX_LIMIT)
}

async fn list_workspaces(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Query(q): Query<PaginationQuery>,
) -> Result<Json<ApiResponse<PaginatedResponse<Workspace>>>, AppError> {
    let limit = clamp_limit(&q);
    let (items, has_more) = state
        .auth_store
        .list_workspaces(&auth.account_id, q.after.as_deref(), limit)
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
    // Must be active: wk_ auth no longer checks workspace.status (the key's
    // own JOIN only validates the account), so issuing a key against an
    // archived workspace would silently re-open its data plane (codex H1).
    let ws = auth.load_owned_workspace(&state, &ws_id).await?;
    if ws.status != veda_types::WorkspaceStatus::Active {
        return Err(VedaError::NotFound("workspace".into()).into());
    }

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
        account_id: ws.account_id,
        name,
        key_hash,
        permission,
        status: KeyStatus::Active,
        kind: ws.kind,
        created_at: now,
    };
    state.auth_store.create_workspace_key(&wk).await?;

    Ok(Json(ApiResponse::ok(serde_json::json!({
        "key": raw_key,
        "permission": perm,
    }))))
}

/// List a workspace's keys (metadata only — `key_hash` is `#[serde(skip)]`,
/// so the plaintext is never re-surfaced; it's shown once at creation).
async fn list_workspace_keys(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(ws_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<WorkspaceKey>>>, AppError> {
    let _ws = auth.load_owned_workspace(&state, &ws_id).await?;
    let keys = state.auth_store.list_workspace_keys(&ws_id).await?;
    Ok(Json(ApiResponse::ok(keys)))
}

/// Revoke a workspace key. `revoke_workspace_key` is unconditional by id, so
/// confirm the key belongs to THIS workspace (which the caller's account
/// owns) first — otherwise knowing a key id would let any account revoke it.
/// Mirrors the ownership guard on admin token disable.
async fn delete_workspace_key(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path((ws_id, key_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let _ws = auth.load_owned_workspace(&state, &ws_id).await?;
    let keys = state.auth_store.list_workspace_keys(&ws_id).await?;
    if !keys.iter().any(|k| k.id == key_id) {
        return Err(VedaError::NotFound(format!("workspace key {key_id}")).into());
    }
    state.auth_store.revoke_workspace_key(&key_id, &ws_id).await?;
    Ok(StatusCode::NO_CONTENT)
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

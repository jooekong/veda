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
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use veda_types::api::CreateWorkspaceRequest;
use veda_core::checksum::sha256_hex;
use veda_types::validate;
use veda_types::{
    Account, AccountStatus, ApiResponse, Dataset, DatasetStatus, KeyPermission, KeyStatus,
    VedaError, Workspace, WorkspaceKey,
};

use crate::error::AppError;
use crate::platform::{resolve_workspace_name, GatewayUser};
use crate::routes::account::create_workspace_under;
use crate::state::AppState;

/// Offset-pagination query for apps list endpoints (company spec): `page` (from
/// 1), `size` (default 20, max 200), `order_by` (`created_at` | `id`), `order`
/// (`asc` | `desc`).
#[derive(serde::Deserialize)]
struct AppPageQuery {
    page: Option<u32>,
    size: Option<u32>,
    order_by: Option<String>,
    order: Option<String>,
}

impl AppPageQuery {
    /// `(page, size, order_by, order)` with defaults + clamps applied.
    fn resolved(&self) -> (u32, u32, String, String) {
        (
            self.page.unwrap_or(1).max(1),
            self.size.unwrap_or(20).clamp(1, 200),
            self.order_by.clone().unwrap_or_else(|| "created_at".into()),
            self.order.clone().unwrap_or_else(|| "desc".into()),
        )
    }
}

/// Company page envelope returned directly by apps list handlers. The response
/// middleware passes it through untouched (it has no `success` field).
#[derive(serde::Serialize)]
struct CompanyPage<T> {
    data: Vec<T>,
    page: u32,
    size: u32,
    order_by: String,
    order: String,
    total: i64,
    total_page: i64,
    has_next_page: bool,
    has_prev_page: bool,
}

impl<T> CompanyPage<T> {
    fn new(data: Vec<T>, page: u32, size: u32, order_by: String, order: String, total: i64) -> Self {
        let total_page = if size > 0 {
            (total + size as i64 - 1) / size as i64
        } else {
            0
        };
        CompanyPage {
            has_next_page: (page as i64) < total_page,
            has_prev_page: page > 1,
            data,
            page,
            size,
            order_by,
            order,
            total,
            total_page,
        }
    }
}

/// Apps-surface workspace view. Renames the internal `app_id` to the platform
/// `workspace_id` at the boundary (item 3) and carries `workspace_name` +
/// creator identity; the internal `Workspace` type stays untouched.
#[derive(serde::Serialize)]
struct AppWorkspace {
    /// Platform workspace code (gateway tenant; stored internally as `app_id`).
    workspace_id: Option<String>,
    /// Platform workspace display name (looked up; null until lookup is wired).
    workspace_name: Option<String>,
    /// veda's own workspace (vector index) id.
    id: String,
    name: String,
    kind: veda_types::WorkspaceKind,
    status: veda_types::WorkspaceStatus,
    description: Option<String>,
    creator: Option<String>,
    creator_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl AppWorkspace {
    fn build(
        ws: Workspace,
        workspace_name: Option<String>,
        creator: Option<String>,
        creator_name: Option<String>,
    ) -> Self {
        AppWorkspace {
            workspace_id: ws.app_id,
            workspace_name,
            id: ws.id,
            name: ws.name,
            kind: ws.kind,
            status: ws.status,
            description: ws.description,
            creator,
            creator_name,
            created_at: ws.created_at,
            updated_at: ws.updated_at,
        }
    }
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
        .route(
            "/v1/apps/{app_id}/workspaces/{id}/keys",
            post(create_app_key).get(list_app_keys),
        )
        .route(
            "/v1/apps/{app_id}/workspaces/{id}/keys/{key_id}",
            delete(revoke_app_key),
        )
        .route(
            "/v1/apps/{app_id}/workspaces/{id}/keys/{key_id}/token",
            get(get_app_key_token),
        )
        .route(
            "/v1/apps/{app_id}/workspaces/{id}/datasets",
            post(create_app_dataset).get(list_app_datasets),
        )
        // Stage 6: company response envelope, apps surface only.
        .layer(axum::middleware::from_fn(company_envelope))
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
    gw: GatewayUser,
    Json(mut req): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<ApiResponse<AppWorkspace>>), AppError> {
    let app_id = require_app_id(&app_id)?.to_string();
    let account = ensure_account(&state, &app_id).await?;
    req.app_id = Some(app_id.clone());
    let ws = create_workspace_under(&state, account.id, req).await?;
    // Stamp creator from the gateway identity (NULL on direct access).
    let creator = gw.creator();
    let creator_name = gw.creator_name();
    state
        .auth_store
        .set_workspace_creator(&ws.id, creator.as_deref(), creator_name.as_deref())
        .await?;
    let workspace_name = resolve_workspace_name(&app_id).await;
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::ok(AppWorkspace::build(
            ws,
            workspace_name,
            creator,
            creator_name,
        ))),
    ))
}

/// GET /v1/apps/{app_id}/workspaces — list active workspaces under the app.
/// A GET must not have side effects, so an unknown app_id returns an empty page
/// rather than auto-provisioning a tenant.
async fn list_app_workspaces(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(q): Query<AppPageQuery>,
) -> Result<Json<CompanyPage<AppWorkspace>>, AppError> {
    let app_id = require_app_id(&app_id)?;
    let (page, size, order_by, order) = q.resolved();
    let account = match lookup_active_account(&state, app_id).await? {
        Some(acc) => acc,
        None => return Ok(Json(CompanyPage::new(Vec::new(), page, size, order_by, order, 0))),
    };
    let offset = (page - 1) * size;
    let (rows, total) = state
        .auth_store
        .list_app_workspaces(&account.id, offset, size, &order_by, &order)
        .await?;
    let workspace_name = resolve_workspace_name(app_id).await;
    let data = rows
        .into_iter()
        .map(|(ws, creator, creator_name)| {
            AppWorkspace::build(ws, workspace_name.clone(), creator, creator_name)
        })
        .collect();
    Ok(Json(CompanyPage::new(data, page, size, order_by, order, total)))
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

// ── Workspace keys (apps surface) ──────────────────────

/// Apps-surface key view. Carries a MASKED token (full token via getToken)
/// plus creator identity; never exposes `key_hash`.
#[derive(serde::Serialize)]
struct AppKey {
    id: String,
    name: String,
    permission: KeyPermission,
    status: KeyStatus,
    /// Masked token for display, e.g. `wk_a1b2…c3d4`. Full token via getToken.
    token: String,
    creator: Option<String>,
    creator_name: Option<String>,
    created_at: DateTime<Utc>,
}

/// Mask a `wk_` token for console display: keep the `wk_` + 4 leading chars and
/// the last 4. Tokens are ASCII (`wk_` + hex) so byte-slicing is safe.
fn mask_token(token: &str) -> String {
    let n = token.len();
    if n <= 12 {
        return "****".to_string();
    }
    format!("{}…{}", &token[..7], &token[n - 4..])
}

/// Resolve + authorize the target veda workspace for an app: the app's account
/// must exist (active) and own the workspace. A missing app, missing workspace,
/// or cross-tenant id all collapse to the same `NOT_FOUND` so a probe can't
/// learn another tenant's workspace ids.
async fn load_app_workspace(
    state: &AppState,
    app_id: &str,
    ws_id: &str,
) -> Result<Workspace, AppError> {
    let app_id = require_app_id(app_id)?;
    let account = lookup_active_account(state, app_id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {ws_id}")))?;
    let ws = state
        .auth_store
        .get_workspace(ws_id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("workspace {ws_id}")))?;
    if ws.account_id != account.id {
        return Err(VedaError::NotFound(format!("workspace {ws_id}")).into());
    }
    Ok(ws)
}

/// POST /v1/apps/{app_id}/workspaces/{id}/keys — mint a data-plane `wk_`.
/// Persists the plaintext token (for getToken) + creator; returns it MASKED.
async fn create_app_key(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<ApiResponse<AppKey>>), AppError> {
    let ws = load_app_workspace(&state, &app_id, &ws_id).await?;
    if ws.status != veda_types::WorkspaceStatus::Active {
        return Err(VedaError::NotFound(format!("workspace {ws_id}")).into());
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let permission = match body
        .get("permission")
        .and_then(|v| v.as_str())
        .unwrap_or("readwrite")
    {
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
    let creator = gw.creator();
    let creator_name = gw.creator_name();
    let wk = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.id.clone(),
        account_id: ws.account_id.clone(),
        name,
        key_hash: sha256_hex(raw_key.as_bytes()),
        permission,
        status: KeyStatus::Active,
        kind: ws.kind,
        created_at: Utc::now(),
    };
    state
        .auth_store
        .create_app_workspace_key(&wk, &raw_key, creator.as_deref(), creator_name.as_deref())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::ok(AppKey {
            id: wk.id,
            name: wk.name,
            permission: wk.permission,
            status: wk.status,
            token: mask_token(&raw_key),
            creator,
            creator_name,
            created_at: wk.created_at,
        })),
    ))
}

/// GET /v1/apps/{app_id}/workspaces/{id}/keys — list keys (masked tokens).
async fn list_app_keys(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<AppKey>>>, AppError> {
    load_app_workspace(&state, &app_id, &ws_id).await?;
    let rows = state.auth_store.list_app_workspace_keys(&ws_id).await?;
    let items = rows
        .into_iter()
        .map(|(k, token, creator, creator_name)| AppKey {
            id: k.id,
            name: k.name,
            permission: k.permission,
            status: k.status,
            token: token
                .as_deref()
                .map(mask_token)
                .unwrap_or_else(|| "****".to_string()),
            creator,
            creator_name,
            created_at: k.created_at,
        })
        .collect();
    Ok(Json(ApiResponse::ok(items)))
}

/// GET /v1/apps/{app_id}/workspaces/{id}/keys/{key_id}/token — reveal the full
/// plaintext token: `{ "token": "wk_..." }`.
async fn get_app_key_token(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id, key_id)): Path<(String, String, String)>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    load_app_workspace(&state, &app_id, &ws_id).await?;
    let token = state
        .auth_store
        .get_workspace_key_token(&key_id, &ws_id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("key {key_id}")))?;
    Ok(Json(ApiResponse::ok(serde_json::json!({ "token": token }))))
}

/// DELETE /v1/apps/{app_id}/workspaces/{id}/keys/{key_id} — revoke a key.
async fn revoke_app_key(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id, key_id)): Path<(String, String, String)>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    load_app_workspace(&state, &app_id, &ws_id).await?;
    state.auth_store.revoke_workspace_key(&key_id).await?;
    Ok(Json(ApiResponse::ok(())))
}

// ── Datasets (apps surface) ────────────────────────────

/// Apps-surface dataset view: carries creator identity; the internal `Dataset`
/// type is untouched.
#[derive(serde::Serialize)]
struct AppDataset {
    id: String,
    name: String,
    status: DatasetStatus,
    description: Option<String>,
    creator: Option<String>,
    creator_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl AppDataset {
    fn build(ds: Dataset, creator: Option<String>, creator_name: Option<String>) -> Self {
        AppDataset {
            id: ds.id,
            name: ds.name,
            status: ds.status,
            description: ds.description,
            creator,
            creator_name,
            created_at: ds.created_at,
            updated_at: ds.updated_at,
        }
    }
}

/// POST /v1/apps/{app_id}/workspaces/{id}/datasets — create a dataset (db ws).
async fn create_app_dataset(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<ApiResponse<AppDataset>>), AppError> {
    let ws = load_app_workspace(&state, &app_id, &ws_id).await?;
    if ws.status != veda_types::WorkspaceStatus::Active {
        return Err(VedaError::NotFound(format!("workspace {ws_id}")).into());
    }
    if ws.kind != veda_types::WorkspaceKind::Db {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    validate::validate_dataset_name(&name)?;
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now = Utc::now();
    let dataset = Dataset {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.id,
        name,
        status: DatasetStatus::Active,
        description,
        created_at: now,
        updated_at: now,
    };
    state.auth_store.create_dataset(&dataset).await?;
    let creator = gw.creator();
    let creator_name = gw.creator_name();
    state
        .auth_store
        .set_dataset_creator(&dataset.id, creator.as_deref(), creator_name.as_deref())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::ok(AppDataset::build(
            dataset,
            creator,
            creator_name,
        ))),
    ))
}

/// GET /v1/apps/{app_id}/workspaces/{id}/datasets — list datasets (with creator).
async fn list_app_datasets(
    State(state): State<Arc<AppState>>,
    Path((app_id, ws_id)): Path<(String, String)>,
    Query(q): Query<AppPageQuery>,
) -> Result<Json<CompanyPage<AppDataset>>, AppError> {
    load_app_workspace(&state, &app_id, &ws_id).await?;
    let (page, size, order_by, order) = q.resolved();
    let offset = (page - 1) * size;
    let (rows, total) = state
        .auth_store
        .list_app_datasets(&ws_id, offset, size, &order_by, &order)
        .await?;
    let data = rows
        .into_iter()
        .map(|(ds, creator, creator_name)| AppDataset::build(ds, creator, creator_name))
        .collect();
    Ok(Json(CompanyPage::new(data, page, size, order_by, order, total)))
}

// ── Company response envelope (stage 6, apps surface only) ──────────────

/// Rewrite veda's native response body into the OnePaaS company format for the
/// AI Workbench frontend, leaving REST status untouched:
///   success → `{ data:[...], page, size, order_by, order, total, total_page,
///               has_next_page, has_prev_page }` (data is always an array;
///               single objects become a one-element array)
///   error   → `{ error: { code, reason, message, external } }`
/// Applied only to the apps router via `from_fn`; the data plane keeps veda's
/// native `{success,data}` / `error_code` shape.
async fn company_envelope(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return axum::response::Response::from_parts(parts, axum::body::Body::empty()),
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        // Non-JSON (e.g. a 204 with empty body) — pass through unchanged.
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    let nb = serde_json::to_vec(&map_to_company(v)).unwrap_or_default();
    // Body length changed; drop the stale Content-Length so it's recomputed.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, axum::body::Body::from(nb))
}

fn map_to_company(v: serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    // Already a company envelope (apps list handlers return `CompanyPage`, which
    // has no `success` field) — pass through untouched, keeping its real
    // page/total. Only veda's `ApiResponse` bodies (always carry `success`) are
    // transformed below.
    if v.get("success").is_none() {
        return v;
    }
    // veda error: `{ success:false, error_code, error }`.
    if v.get("success").and_then(|s| s.as_bool()) == Some(false) {
        let code = v
            .get("error_code")
            .and_then(|x| x.as_str())
            .unwrap_or("INTERNAL");
        let message = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
        return json!({ "error": { "code": code, "reason": "", "message": message, "external": {} } });
    }
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    if let Some(obj) = data.as_object() {
        // veda PaginatedResponse: `{ items, has_more, next_cursor }`.
        if let Some(items) = obj.get("items").and_then(|i| i.as_array()) {
            let has_more = obj.get("has_more").and_then(|b| b.as_bool()).unwrap_or(false);
            return company_page(items.clone(), has_more);
        }
        return company_page(vec![data], false); // single object
    }
    match data {
        serde_json::Value::Array(arr) => company_page(arr, false), // bare list (keys)
        serde_json::Value::Null => company_page(vec![], false),    // 200-with-null (delete)
        other => company_page(vec![other], false),
    }
}

/// Build the company page envelope. `total` / `page` are best-effort (the
/// current page's count) — exact totals need offset+count, which veda's cursor
/// lists don't carry; `has_next_page` is authoritative from the cursor's
/// `has_more`. Apps tenants are typically single-page, so this is accurate in
/// the common case; exact offset pagination is a follow-up.
fn company_page(data: Vec<serde_json::Value>, has_more: bool) -> serde_json::Value {
    let size = data.len();
    serde_json::json!({
        "data": data,
        "page": 1,
        "size": size,
        "order_by": "created_at",
        "order": "desc",
        "total": size,
        "total_page": 1,
        "has_next_page": has_more,
        "has_prev_page": false,
    })
}

#[cfg(test)]
mod tests {
    use super::{map_to_company, mask_token};
    use serde_json::json;

    #[test]
    fn envelope_error_to_error_object() {
        let out = map_to_company(json!({
            "success": false, "error_code": "INVALID_INPUT", "error": "text: empty"
        }));
        assert_eq!(out["error"]["code"], "INVALID_INPUT");
        assert_eq!(out["error"]["message"], "text: empty");
        assert_eq!(out["error"]["reason"], "");
        assert!(out.get("data").is_none(), "error body carries no data");
    }

    #[test]
    fn envelope_single_object_becomes_one_element_array() {
        let out = map_to_company(json!({ "success": true, "data": { "id": "w1", "kind": "db" } }));
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["data"][0]["id"], "w1");
        assert_eq!(out["page"], 1);
        assert_eq!(out["total"], 1);
        assert_eq!(out["has_next_page"], false);
        assert!(out.get("success").is_none(), "no success field in company envelope");
    }

    #[test]
    fn envelope_paginated_drops_cursor_keeps_has_next() {
        let out = map_to_company(json!({
            "success": true,
            "data": { "items": [{"a":1},{"b":2}], "has_more": true, "next_cursor": "c" }
        }));
        assert_eq!(out["data"].as_array().unwrap().len(), 2);
        assert_eq!(out["size"], 2);
        assert_eq!(out["has_next_page"], true);
        assert!(out.get("next_cursor").is_none(), "cursor not leaked into envelope");
        assert!(out["data"][0].get("items").is_none(), "items unwrapped, not nested");
    }

    #[test]
    fn envelope_bare_array_passes_through() {
        let out = map_to_company(json!({ "success": true, "data": [{"k":1}] }));
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["total"], 1);
    }

    #[test]
    fn envelope_null_data_is_empty_array() {
        let out = map_to_company(json!({ "success": true, "data": null }));
        assert_eq!(out["data"].as_array().unwrap().len(), 0);
        assert_eq!(out["total"], 0);
    }

    #[test]
    fn mask_keeps_head_and_tail_hides_middle() {
        let t = format!("wk_{}", "a".repeat(32));
        let m = mask_token(&t);
        assert!(m.starts_with("wk_a") && m.ends_with("aaaa") && m.contains('…'));
        assert!(!m.contains(&t[7..t.len() - 4]), "middle stays hidden");
    }

    #[test]
    fn mask_short_token_fully_masked() {
        assert_eq!(mask_token("wk_short"), "****");
    }
}

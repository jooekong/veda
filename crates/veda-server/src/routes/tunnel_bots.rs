//! WeCom tunnel bot management on the apps surface
//! (`/v1/workspace/{workspace}/project/{id}/tunnel/bots`).
//!
//! Lets the AI Workbench attach a WeCom bot to an **fs** project: the caller
//! supplies the bot's WeCom credentials, veda auto-mints a read-only `wk_`
//! for the project and writes the bot row into `veda_tunnel_bots` (the table
//! shared with veda-tunnel — see `crate::tunnel_bots`). The tunnel process
//! picks changes up within one store-poll interval (~30s); `conn_state` in
//! responses is tunnel's heartbeat written back through the same table.
//!
//! Same posture as the rest of the apps surface: gateway-externalized authz,
//! company response envelope, cross-tenant probes collapse to NOT_FOUND.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_types::{ApiResponse, KeyPermission, KeyStatus, VedaError, WorkspaceKey};

use crate::error::AppError;
use crate::platform::GatewayUser;
use crate::routes::apps::{company_envelope, load_app_project, mask_token};
use crate::state::AppState;
use crate::tunnel_bots::{NewTunnelBot, TunnelBotPatch, TunnelBotRow};

/// WeCom answers cap out fast; keep limit in the same band as /v1/answer.
const MAX_LIMIT: i32 = 24;
const DEFAULT_LIMIT: i32 = 8;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/workspace/{workspace}/project/{id}/tunnel/bots",
            post(create_bot).get(list_bots),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/tunnel/bots/{bot_id}",
            patch(update_bot).delete(remove_bot),
        )
        .layer(axum::middleware::from_fn(company_envelope))
}

/// Platform view of a bot. No secret (write-only), key shown masked.
#[derive(serde::Serialize)]
struct AppTunnelBot {
    bot_id: String,
    name: String,
    /// Platform workspace code (from the URL at creation).
    workspace: String,
    /// veda project id this bot serves.
    project: Option<String>,
    mode: String,
    limit: i32,
    /// Custom answer persona; absent = server default. Round-trips in full
    /// (not a secret) so the workbench edit form can prefill it.
    prompt: Option<String>,
    /// Masked auto-minted read-only key, e.g. `wk_a1b2…c3d4`.
    veda_key: String,
    /// tunnel's connection heartbeat: unknown|connecting|subscribed|
    /// reconnecting|down. `unknown` until the first poll after creation.
    conn_state: String,
    conn_updated_at: Option<DateTime<Utc>>,
    creator: Option<String>,
    creator_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<TunnelBotRow> for AppTunnelBot {
    fn from(r: TunnelBotRow) -> Self {
        AppTunnelBot {
            bot_id: r.bot_id,
            name: r.name,
            workspace: r.workspace,
            project: r.project,
            mode: r.mode,
            limit: r.search_limit,
            prompt: r.prompt,
            veda_key: mask_token(&r.veda_key),
            conn_state: r.conn_state,
            conn_updated_at: r.conn_updated_at,
            creator: r.creator,
            creator_name: r.creator_name,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateBotReq {
    bot_id: String,
    name: String,
    secret: String,
    mode: Option<String>,
    limit: Option<i32>,
    /// Custom answer persona (≤4000 chars); absent/empty = server default.
    prompt: Option<String>,
}

#[derive(serde::Deserialize)]
struct PatchBotReq {
    name: Option<String>,
    /// Empty or absent = keep the stored secret.
    secret: Option<String>,
    mode: Option<String>,
    limit: Option<i32>,
    /// Absent = keep; empty string = clear back to the server default;
    /// non-empty (≤4000 chars) = set.
    prompt: Option<String>,
}

/// Mirror of the `/v1/answer` prompt cap.
const MAX_PROMPT_CHARS: usize = 4000;

fn validate_prompt(prompt: Option<&str>) -> Result<(), AppError> {
    if prompt.is_some_and(|p| p.chars().count() > MAX_PROMPT_CHARS) {
        return Err(VedaError::InvalidInput(format!(
            "prompt must be at most {MAX_PROMPT_CHARS} characters"
        ))
        .into());
    }
    Ok(())
}

fn validate_mode(mode: &str) -> Result<(), AppError> {
    if !matches!(mode, "hybrid" | "semantic" | "fulltext") {
        return Err(VedaError::InvalidInput(format!(
            "invalid mode '{mode}' (want hybrid|semantic|fulltext)"
        ))
        .into());
    }
    Ok(())
}

fn validate_limit(limit: i32) -> Result<(), AppError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(
            VedaError::InvalidInput(format!("limit must be 1..={MAX_LIMIT}, got {limit}")).into(),
        );
    }
    Ok(())
}

/// Resolve the target project and require it to be an active **fs** project —
/// tunnel bots answer from the file knowledge base; db projects have no
/// answerable content.
async fn load_fs_project(
    state: &AppState,
    workspace: &str,
    ws_id: &str,
) -> Result<veda_types::Workspace, AppError> {
    let ws = load_app_project(state, workspace, ws_id).await?;
    if ws.status != veda_types::WorkspaceStatus::Active {
        return Err(VedaError::NotFound(format!("project {ws_id}")).into());
    }
    if ws.kind != veda_types::WorkspaceKind::Fs {
        return Err(VedaError::WorkspaceKindMismatch.into());
    }
    Ok(ws)
}

/// POST /v1/workspace/{workspace}/project/{id}/tunnel/bots — attach a WeCom
/// bot to this fs project. Mints a dedicated read-only `wk_` behind the
/// scenes; the caller never handles veda keys.
async fn create_bot(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id)): Path<(String, String)>,
    gw: GatewayUser,
    Json(req): Json<CreateBotReq>,
) -> Result<(StatusCode, Json<ApiResponse<AppTunnelBot>>), AppError> {
    crate::platform::authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?;
    let ws = load_fs_project(&state, &workspace, &ws_id).await?;

    let bot_id = req.bot_id.trim().to_string();
    let name = req.name.trim().to_string();
    for (field, v) in [("bot_id", &bot_id), ("name", &name), ("secret", &req.secret)] {
        if v.trim().is_empty() {
            return Err(VedaError::InvalidInput(format!("{field} is required")).into());
        }
    }
    let mode = req.mode.unwrap_or_else(|| "hybrid".to_string());
    validate_mode(&mode)?;
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    validate_limit(limit)?;
    validate_prompt(req.prompt.as_deref())?;
    // Empty persona = no persona (server default).
    let prompt = req.prompt.filter(|p| !p.trim().is_empty());

    // Mint the bot's dedicated read-only data-plane key.
    let raw_key = format!("wk_{}", Uuid::new_v4().to_string().replace('-', ""));
    let creator = gw.creator();
    let creator_name = gw.creator_name();
    let wk = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.id.clone(),
        account_id: ws.account_id.clone(),
        name: format!("tunnel-{name}"),
        key_hash: sha256_hex(raw_key.as_bytes()),
        permission: KeyPermission::Read,
        status: KeyStatus::Active,
        kind: ws.kind,
        created_at: Utc::now(),
    };
    state
        .auth_store
        .create_app_workspace_key(&wk, &raw_key, creator.as_deref(), creator_name.as_deref())
        .await?;

    let new = NewTunnelBot {
        bot_id: bot_id.clone(),
        name,
        secret: req.secret,
        veda_key: raw_key,
        workspace: workspace.clone(),
        project: ws.id.clone(),
        mode,
        search_limit: limit,
        prompt,
        key_id: wk.id.clone(),
        creator,
        creator_name,
    };
    if let Err(e) = state.tunnel_bots.insert(&new).await {
        // Don't leak an active key when the row loses a uniqueness race. A
        // failed rollback leaves an orphaned (read-only, tenant-scoped) key —
        // log it so ops can revoke by hand; not worth a cross-store txn.
        if let Err(re) = state.auth_store.revoke_workspace_key(&wk.id, &ws.id).await {
            tracing::warn!(key_id = %wk.id, project = %ws.id, error = %re,
                "tunnel bot create conflicted AND key rollback failed — revoke manually");
        }
        return Err(e.into());
    }

    let view = state
        .tunnel_bots
        .get_in_project(&bot_id, &ws.id)
        .await?
        .ok_or_else(|| VedaError::Internal("bot vanished after insert".into()))?;
    Ok((StatusCode::OK, Json(ApiResponse::ok(view.into()))))
}

/// GET /v1/workspace/{workspace}/project/{id}/tunnel/bots — bots attached to
/// this project (typically 0–2; no pagination, mirrors the keys list).
async fn list_bots(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<AppTunnelBot>>>, AppError> {
    load_fs_project(&state, &workspace, &ws_id).await?;
    let rows = state.tunnel_bots.list_by_project(&ws_id).await?;
    Ok(Json(ApiResponse::ok(
        rows.into_iter().map(Into::into).collect(),
    )))
}

/// PATCH /v1/workspace/{workspace}/project/{id}/tunnel/bots/{bot_id} — update
/// name/secret/mode/limit. Absent (or empty, for secret) fields keep their
/// stored values; bot_id is immutable. Tunnel re-connects the bot within one
/// poll interval when anything material changed.
async fn update_bot(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id, bot_id)): Path<(String, String, String)>,
    gw: GatewayUser,
    Json(req): Json<PatchBotReq>,
) -> Result<Json<ApiResponse<AppTunnelBot>>, AppError> {
    crate::platform::authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?;
    let ws = load_fs_project(&state, &workspace, &ws_id).await?;
    if let Some(m) = req.mode.as_deref() {
        validate_mode(m)?;
    }
    if let Some(l) = req.limit {
        validate_limit(l)?;
    }
    if let Some(n) = req.name.as_deref() {
        if n.trim().is_empty() {
            return Err(VedaError::InvalidInput("name must not be blank".into()).into());
        }
    }
    validate_prompt(req.prompt.as_deref())?;
    let patch = TunnelBotPatch {
        name: req.name.map(|s| s.trim().to_string()),
        secret: req.secret,
        mode: req.mode,
        search_limit: req.limit,
        prompt: req.prompt,
    };
    if !state
        .tunnel_bots
        .update_in_project(&bot_id, &ws.id, &patch)
        .await?
    {
        return Err(VedaError::NotFound(format!("bot {bot_id}")).into());
    }
    let view = state
        .tunnel_bots
        .get_in_project(&bot_id, &ws.id)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("bot {bot_id}")))?;
    Ok(Json(ApiResponse::ok(view.into())))
}

/// DELETE /v1/workspace/{workspace}/project/{id}/tunnel/bots/{bot_id} —
/// detach the bot and revoke its auto-minted key. Tunnel drops the WeCom
/// connection within one poll interval.
async fn remove_bot(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id, bot_id)): Path<(String, String, String)>,
    gw: GatewayUser,
) -> Result<Json<ApiResponse<()>>, AppError> {
    crate::platform::authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?;
    let ws = load_fs_project(&state, &workspace, &ws_id).await?;
    match state.tunnel_bots.delete_in_project(&bot_id, &ws.id).await? {
        None => Err(VedaError::NotFound(format!("bot {bot_id}")).into()),
        Some(key_id) => {
            // Best-effort revoke of the key minted at create; bots added via
            // tunnel's own admin console have no key_id and are left alone.
            // On failure the bot row is already gone, so log loud enough for
            // ops to revoke the orphan by hand (read-only, tenant-scoped).
            if let Some(kid) = key_id {
                if let Err(re) = state.auth_store.revoke_workspace_key(&kid, &ws.id).await {
                    tracing::warn!(key_id = %kid, project = %ws.id, error = %re,
                        "bot deleted but key revoke failed — revoke manually");
                }
            }
            Ok(Json(ApiResponse::ok(())))
        }
    }
}

//! WeCom tunnel bot management + QA telemetry on the apps surface
//! (`/v1/workspace/{workspace}/project/{id}/tunnel/...`).
//!
//! Lets the AI Workbench attach a WeCom bot to an **fs** project: the caller
//! supplies the bot's WeCom credentials, veda auto-mints a read-only `wk_`
//! for the project and writes the bot row into `veda_tunnel_bots` (the table
//! shared with veda-tunnel — see `crate::tunnel_bots`). The tunnel process
//! picks changes up within one store-poll interval (~30s); `conn_state` in
//! responses is tunnel's heartbeat written back through the same table.
//!
//! `tunnel/qa/{stats,logs}` expose the tunnel's per-project QA telemetry
//! (question/answer outcomes + thumb up/down). The telemetry tables key only
//! on `bot_id`, so tenant isolation is enforced here: every read resolves the
//! project's own bots first and constrains the query to that `bot_id` set — a
//! caller-supplied `bot_id` outside it collapses to NOT_FOUND.
//!
//! Same posture as the rest of the apps surface: gateway-externalized authz,
//! company response envelope, cross-tenant probes collapse to NOT_FOUND.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_types::{ApiResponse, KeyPermission, KeyStatus, VedaError, WorkspaceKey};

use crate::error::AppError;
use crate::platform::GatewayUser;
use crate::routes::apps::{company_envelope, load_app_project, mask_token, CompanyPage};
use crate::state::AppState;
use crate::tunnel_bots::{NewTunnelBot, QaLogRow, QaStats, TunnelBotPatch, TunnelBotRow};

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
        .route(
            "/v1/workspace/{workspace}/project/{id}/tunnel/qa/stats",
            get(qa_stats),
        )
        .route(
            "/v1/workspace/{workspace}/project/{id}/tunnel/qa/logs",
            get(qa_logs),
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

// ── QA telemetry (read-only) ────────────────────────────────────────────────

// QA read parameter bounds (the store re-clamps `size` defensively).
const QA_DEFAULT_DAYS: u32 = 7;
const QA_MAX_DAYS: u32 = 90;
const QA_DEFAULT_SIZE: u32 = 20;
const QA_MAX_SIZE: u32 = 100;

/// Outcomes the tunnel currently records — keep in sync with
/// `veda-tunnel/src/wecom/handler.rs` (answer + search + error paths). A filter
/// outside this set is a typo, so reject it early instead of silently
/// returning nothing.
const KNOWN_OUTCOMES: [&str; 8] = [
    "answered",
    "no_context",
    "ungrounded",
    "raw_search",
    "error",
    "upstream_error",
    "disabled",
    "throttled",
];

fn clamp_days(days: Option<u32>) -> u32 {
    days.unwrap_or(QA_DEFAULT_DAYS).clamp(1, QA_MAX_DAYS)
}

fn clamp_page(page: Option<u32>) -> u32 {
    page.unwrap_or(1).max(1)
}

fn clamp_size(size: Option<u32>) -> u32 {
    size.unwrap_or(QA_DEFAULT_SIZE).clamp(1, QA_MAX_SIZE)
}

fn validate_outcome(outcome: &str) -> Result<(), AppError> {
    if KNOWN_OUTCOMES.contains(&outcome) {
        Ok(())
    } else {
        Err(VedaError::InvalidInput(format!(
            "unknown outcome '{outcome}' (want {})",
            KNOWN_OUTCOMES.join("|")
        ))
        .into())
    }
}

/// Resolve the `bot_id` scope a QA read may touch — the crux of tenant
/// isolation, since the QA tables carry no workspace/project column.
///
/// The scope is the project's own bots. A caller-supplied `bot_id` must be one
/// of them — otherwise NOT_FOUND, so a probe can't learn another tenant's bot
/// exists (same cross-tenant posture as the rest of this surface). A project
/// with no bots yields an empty scope → empty stats / logs, never an error.
async fn resolve_bot_scope(
    state: &AppState,
    project_id: &str,
    bot_id: Option<&str>,
) -> Result<Vec<String>, AppError> {
    let owned = state.tunnel_bots.bot_ids_by_project(project_id).await?;
    match bot_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(want) => {
            if owned.iter().any(|b| b == want) {
                Ok(vec![want.to_string()])
            } else {
                Err(VedaError::NotFound(format!("bot {want}")).into())
            }
        }
        None => Ok(owned),
    }
}

#[derive(serde::Deserialize)]
struct QaStatsQuery {
    days: Option<u32>,
    bot_id: Option<String>,
}

/// GET /v1/workspace/{workspace}/project/{id}/tunnel/qa/stats?days=7&bot_id= —
/// outcome distribution + thumb up/down over the window, scoped to this
/// project's bots. `days` defaults to 7, clamped to 1..=90; an optional
/// `bot_id` must belong to the project.
async fn qa_stats(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<QaStatsQuery>,
) -> Result<Json<ApiResponse<QaStats>>, AppError> {
    // QA rows carry user questions + bot answers (actual content), so gate the
    // read with the same external authz as the data plane (project_data.rs),
    // not the lighter bot-list posture.
    crate::platform::authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?;
    let ws = load_fs_project(&state, &workspace, &ws_id).await?;
    let days = clamp_days(q.days);
    let scope = resolve_bot_scope(&state, &ws.id, q.bot_id.as_deref()).await?;
    let stats = state.tunnel_bots.qa_stats(&scope, days).await?;
    Ok(Json(ApiResponse::ok(stats)))
}

#[derive(serde::Deserialize)]
struct QaLogsQuery {
    bot_id: Option<String>,
    outcome: Option<String>,
    /// Only rows with at least one down-vote.
    down_voted: Option<bool>,
    page: Option<u32>,
    size: Option<u32>,
}

/// GET /v1/workspace/{workspace}/project/{id}/tunnel/qa/logs?bot_id=&outcome=&
/// down_voted=&page=&size= — newest-first Q&A rows with per-row vote counts,
/// paginated in the company envelope. `size` defaults to 20, clamped to
/// 1..=100; `page` is 1-based. Scoped to this project's bots.
async fn qa_logs(
    State(state): State<Arc<AppState>>,
    Path((workspace, ws_id)): Path<(String, String)>,
    gw: GatewayUser,
    Query(q): Query<QaLogsQuery>,
) -> Result<Json<CompanyPage<QaLogRow>>, AppError> {
    crate::platform::authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?;
    let ws = load_fs_project(&state, &workspace, &ws_id).await?;
    let page = clamp_page(q.page);
    let size = clamp_size(q.size);
    let outcome = q.outcome.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(o) = outcome {
        validate_outcome(o)?;
    }
    let down_voted = q.down_voted.unwrap_or(false);
    let scope = resolve_bot_scope(&state, &ws.id, q.bot_id.as_deref()).await?;
    let (rows, total) = state
        .tunnel_bots
        .qa_logs(&scope, outcome, down_voted, page, size)
        .await?;
    Ok(Json(CompanyPage::new(
        rows,
        page,
        size,
        "ts".into(),
        "desc".into(),
        total,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_defaults_and_clamps() {
        assert_eq!(clamp_days(None), 7);
        assert_eq!(clamp_days(Some(0)), 1);
        assert_eq!(clamp_days(Some(1)), 1);
        assert_eq!(clamp_days(Some(30)), 30);
        assert_eq!(clamp_days(Some(90)), 90);
        assert_eq!(clamp_days(Some(9999)), 90);
    }

    #[test]
    fn page_defaults_to_one_and_never_zero() {
        assert_eq!(clamp_page(None), 1);
        assert_eq!(clamp_page(Some(0)), 1);
        assert_eq!(clamp_page(Some(5)), 5);
    }

    #[test]
    fn size_defaults_and_clamps() {
        assert_eq!(clamp_size(None), 20);
        assert_eq!(clamp_size(Some(0)), 1);
        assert_eq!(clamp_size(Some(50)), 50);
        assert_eq!(clamp_size(Some(100)), 100);
        assert_eq!(clamp_size(Some(101)), 100);
    }

    #[test]
    fn outcome_known_accepted_unknown_rejected() {
        for o in KNOWN_OUTCOMES {
            assert!(validate_outcome(o).is_ok(), "{o} should be valid");
        }
        let err = validate_outcome("bogus").unwrap_err();
        assert!(matches!(err.0, VedaError::InvalidInput(_)));
    }
}

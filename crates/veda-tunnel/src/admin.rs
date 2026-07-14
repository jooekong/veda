//! Fail-closed admin surface (§10): bot CRUD (backed by the MySQL store) +
//! runtime status + intervention (reconnect / reload). Mirrors veda-server's
//! admin posture — an unset token makes every `/admin/*` route 404
//! (discloses nothing); a wrong token is 401. `[admin].listen` defaults to
//! 127.0.0.1 so ops reach it over SSH / an nginx reverse proxy.
//!
//! CRUD handlers persist to the store via the control loop (which also
//! spawns/stops live connections) so the DB and the running fleet never
//! drift. `secret` is never returned; `veda_key` is masked.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::config::BotConfig;
use crate::qa_log::{QaLogFilter, QaLogStore};
use crate::registry::{self, BotStatus, ConnState, Registry};
use crate::store::BotStore;

/// Commands from the admin surface to main's control loop. The loop owns bot
/// task lifecycles + the store; handlers only send intents and await a reply.
pub enum ControlCmd {
    Reconnect {
        bot_id: String,
        reply: oneshot::Sender<bool>,
    },
    Reload {
        reply: oneshot::Sender<Result<usize, String>>,
    },
    AddBot {
        bot: BotConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },
    UpdateBot {
        bot: BotConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveBot {
        bot_id: String,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Clone)]
pub struct AdminState {
    pub registry: Registry,
    pub store: Arc<BotStore>,
    pub qa_log: Arc<QaLogStore>,
    pub admin_token: Option<String>,
    pub control: mpsc::Sender<ControlCmd>,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/admin/bots", get(list_bots).post(add_bot))
        .route(
            "/admin/bots/{bot_id}",
            get(get_bot).put(update_bot).delete(remove_bot),
        )
        .route("/admin/bots/{bot_id}/reconnect", post(reconnect))
        .route("/admin/reload", post(reload))
        .route("/admin/stats", get(qa_stats))
        .route("/admin/qa-log", get(qa_log_list))
        .with_state(state)
}

/// Liveness probe — intentionally unauthenticated.
async fn healthz() -> &'static str {
    "ok"
}

// ── Read ────────────────────────────────────────────────

async fn list_bots(_: AdminAuth, State(st): State<AdminState>) -> Response {
    match st.store.list().await {
        Ok(bots) => {
            let views: Vec<BotView> = bots
                .iter()
                .map(|b| BotView::build(b, registry::get(&st.registry, &b.bot_id)))
                .collect();
            Json(views).into_response()
        }
        Err(e) => internal(e.to_string()),
    }
}

async fn get_bot(_: AdminAuth, State(st): State<AdminState>, Path(bot_id): Path<String>) -> Response {
    match st.store.get(&bot_id).await {
        Ok(Some(b)) => {
            Json(BotView::build(&b, registry::get(&st.registry, &bot_id))).into_response()
        }
        Ok(None) => not_found(),
        Err(e) => internal(e.to_string()),
    }
}

// ── Write ───────────────────────────────────────────────

async fn add_bot(_: AdminAuth, State(st): State<AdminState>, Json(bot): Json<BotConfig>) -> Response {
    if let Err(e) = bot.validate() {
        return bad_request(e.to_string());
    }
    if bot.secret.trim().is_empty() {
        return bad_request("secret is required".to_string());
    }
    if bot.veda_key.trim().is_empty() {
        return bad_request("veda_key is required".to_string());
    }
    let (tx, rx) = oneshot::channel();
    if st
        .control
        .send(ControlCmd::AddBot { bot, reply: tx })
        .await
        .is_err()
    {
        return unavailable();
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::CREATED, Json(json!({"status":"created"}))).into_response(),
        // Duplicate bot_id / name.
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(json!({"error": e}))).into_response(),
        Err(_) => unavailable(),
    }
}

async fn update_bot(
    _: AdminAuth,
    State(st): State<AdminState>,
    Path(bot_id): Path<String>,
    Json(mut bot): Json<BotConfig>,
) -> Response {
    // Path is authoritative for identity; an empty `secret` means "keep".
    bot.bot_id = bot_id;
    if let Err(e) = bot.validate() {
        return bad_request(e.to_string());
    }
    let (tx, rx) = oneshot::channel();
    if st
        .control
        .send(ControlCmd::UpdateBot { bot, reply: tx })
        .await
        .is_err()
    {
        return unavailable();
    }
    match rx.await {
        Ok(Ok(())) => Json(json!({"status":"updated"})).into_response(),
        Ok(Err(e)) if e.contains("unknown") => not_found(),
        Ok(Err(e)) => bad_request(e),
        Err(_) => unavailable(),
    }
}

async fn remove_bot(
    _: AdminAuth,
    State(st): State<AdminState>,
    Path(bot_id): Path<String>,
) -> Response {
    let (tx, rx) = oneshot::channel();
    if st
        .control
        .send(ControlCmd::RemoveBot { bot_id, reply: tx })
        .await
        .is_err()
    {
        return unavailable();
    }
    match rx.await {
        Ok(true) => Json(json!({"status":"deleted"})).into_response(),
        Ok(false) => not_found(),
        Err(_) => unavailable(),
    }
}

// ── Intervention ────────────────────────────────────────

async fn reconnect(
    _: AdminAuth,
    State(st): State<AdminState>,
    Path(bot_id): Path<String>,
) -> Response {
    let (tx, rx) = oneshot::channel();
    if st
        .control
        .send(ControlCmd::Reconnect {
            bot_id: bot_id.clone(),
            reply: tx,
        })
        .await
        .is_err()
    {
        return unavailable();
    }
    match rx.await {
        Ok(true) => Json(json!({"status":"reconnecting","bot_id":bot_id})).into_response(),
        Ok(false) => not_found(),
        Err(_) => unavailable(),
    }
}

async fn reload(_: AdminAuth, State(st): State<AdminState>) -> Response {
    let (tx, rx) = oneshot::channel();
    if st
        .control
        .send(ControlCmd::Reload { reply: tx })
        .await
        .is_err()
    {
        return unavailable();
    }
    match rx.await {
        Ok(Ok(n)) => Json(json!({"status":"reloaded","bots":n})).into_response(),
        Ok(Err(e)) => internal(e),
        Err(_) => unavailable(),
    }
}

// ── QA telemetry (docs/plans/veda-tunnel-qa-log.md) ─────

#[derive(serde::Deserialize)]
struct StatsQuery {
    days: Option<u32>,
    bot_id: Option<String>,
}

/// GET /admin/stats?days=7&bot_id= — totals + outcome distribution +
/// thumb up/down counts over the window.
async fn qa_stats(_: AdminAuth, State(st): State<AdminState>, Query(q): Query<StatsQuery>) -> Response {
    let days = q.days.unwrap_or(7).clamp(1, 90);
    match st.qa_log.stats(days, q.bot_id.as_deref()).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct QaLogQuery {
    outcome: Option<String>,
    /// "1"/"true" → only rows with at least one down-vote.
    down_voted: Option<bool>,
    bot_id: Option<String>,
    page: Option<u32>,
    size: Option<u32>,
}

/// GET /admin/qa-log?outcome=&down_voted=&page= — newest-first Q&A rows with
/// per-row vote counts; the bad-case browsing surface.
async fn qa_log_list(
    _: AdminAuth,
    State(st): State<AdminState>,
    Query(q): Query<QaLogQuery>,
) -> Response {
    let filter = QaLogFilter {
        outcome: q.outcome.filter(|s| !s.is_empty()),
        down_voted: q.down_voted.unwrap_or(false),
        bot_id: q.bot_id.filter(|s| !s.is_empty()),
        page: q.page.unwrap_or(1),
        size: q.size.unwrap_or(20),
    };
    match st.qa_log.list(&filter).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal(e.to_string()),
    }
}

// ── Response helpers ────────────────────────────────────

fn bad_request(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error":"unknown bot_id"}))).into_response()
}
fn internal(msg: String) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": msg }))).into_response()
}
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error":"control loop down"})),
    )
        .into_response()
}

// ── View (config + status, secret stripped, key masked) ──

fn mask_key(s: &str) -> String {
    let n = s.chars().count();
    if n <= 10 {
        return "****".to_string();
    }
    let head: String = s.chars().take(6).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

#[derive(Serialize)]
struct BotView {
    name: String,
    bot_id: String,
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    mode: String,
    limit: usize,
    /// Custom answer persona; absent = server default. Round-trips in full
    /// so the edit form can prefill it (not a secret, unlike veda_key).
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    /// Masked (`wk_b36…0f58`) — the plaintext key never leaves the store.
    veda_key_masked: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conn_state: Option<ConnState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    connected_since: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_msg_at: Option<DateTime<Utc>>,
    msg_count: u64,
    error_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl BotView {
    fn build(b: &BotConfig, st: Option<BotStatus>) -> Self {
        let mut v = Self {
            name: b.name.clone(),
            bot_id: b.bot_id.clone(),
            workspace: b.workspace.clone(),
            project: b.project.clone(),
            mode: b.mode.clone(),
            limit: b.limit,
            prompt: b.prompt.clone(),
            veda_key_masked: mask_key(&b.veda_key),
            conn_state: None,
            connected_since: None,
            last_msg_at: None,
            msg_count: 0,
            error_count: 0,
            last_error: None,
        };
        if let Some(s) = st {
            v.conn_state = Some(s.conn_state);
            v.connected_since = s.connected_since;
            v.last_msg_at = s.last_msg_at;
            v.msg_count = s.msg_count;
            v.error_count = s.error_count;
            v.last_error = s.last_error;
        }
        v
    }
}

// ── Auth ────────────────────────────────────────────────

/// Bearer-token gate. Fail-closed: token unset → 404 (route looks absent);
/// token set but bearer missing/wrong → 401. Constant-time comparison.
struct AdminAuth;

impl FromRequestParts<AdminState> for AdminAuth {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AdminState,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let expected = state.admin_token.clone();
        let presented = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::to_string);
        async move {
            let Some(expected) = expected.filter(|t| !t.is_empty()) else {
                return Err(StatusCode::NOT_FOUND.into_response());
            };
            match presented {
                Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => Ok(AdminAuth),
                _ => Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response()),
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, mask_key};

    #[test]
    fn ct_eq_matches_and_differs() {
        assert!(constant_time_eq(b"tok", b"tok"));
        assert!(!constant_time_eq(b"tok", b"toz"));
        assert!(!constant_time_eq(b"tok", b"tokk"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn masks_key_middle() {
        assert_eq!(mask_key("wk_b3601130416940e3b7f51560dbfe0f58"), "wk_b36…0f58");
        assert_eq!(mask_key("short"), "****");
    }
}

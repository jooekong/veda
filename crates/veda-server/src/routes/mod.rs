pub mod account;
pub mod admin;
pub mod admin_tokens;
pub mod answer;
pub mod apps;
pub mod collection;
pub mod datasets;
pub mod events;
pub mod fs;
pub mod mcp;
pub mod project_data;
pub mod reconcile;
pub mod search;
pub mod sql;
pub mod tunnel_bots;
pub mod vectors;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::timeout::TimeoutLayer;
use veda_types::ApiResponse;

const READY_TIMEOUT: Duration = Duration::from_secs(3);

// install.sh embedded at build time. Updates ship via redeploy — the
// served script is pinned to whatever was in the repo when this binary
// was built. Path is relative to this source file; 4 levels up = repo root.
const INSTALL_SH: &str = include_str!("../../../../install.sh");

pub fn build_router(state: Arc<AppState>) -> Router {
    // Everything except the SSE stream gets a wall-clock request timeout so a
    // runaway query / hung upstream can't pin a request task forever. `/v1/events`
    // is a long-lived SSE stream — a 30s timeout would cut every client off, so
    // it's merged in *after* the layer and runs untimed.
    let timed = Router::new()
        .route("/healthz", get(healthz))
        .route("/install.sh", get(install_script))
        .route("/capabilities", get(capabilities))
        .route("/v1/ready", get(ready))
        .route("/v1/whoami", get(whoami))
        .route("/v1/metrics", get(metrics_endpoint))
        .merge(account::routes())
        .merge(apps::routes())
        .merge(tunnel_bots::routes())
        .merge(admin_tokens::routes())
        .merge(admin::routes())
        .merge(reconcile::routes())
        .merge(datasets::routes())
        .merge(vectors::routes())
        .merge(project_data::routes())
        .merge(fs::routes())
        .merge(search::routes())
        .merge(collection::routes())
        .merge(sql::routes())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ));

    timed
        .merge(events::routes())
        // `/v1/answer` carries its own 90s deadline (the agentic tool loop
        // budgets 80s internally), so it must NOT sit under the 30s
        // TimeoutLayer above — that would cut a legitimate answer off
        // mid-loop. Same rationale as the SSE stream: merged in after the
        // layer, untimed at the tower level, self-limited inside the handler.
        .merge(answer::routes())
        // `/mcp` hosts the `ask` tool (same 90s budget as /v1/answer), so it
        // also lives outside the 30s layer; every tool call carries its own
        // in-handler timeout (routes/mcp.rs::TOOL_TIMEOUT / ASK_TOOL_TIMEOUT).
        .merge(mcp::routes())
        .with_state(state)
}

async fn metrics_endpoint(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !metrics_auth_ok(state.metrics_token.as_deref(), &headers) {
        // Match how the endpoint behaves when disabled: don't disclose
        // existence on bad/missing tokens. Prometheus operators see the same
        // 404 whether the endpoint isn't configured or their token is wrong;
        // they fix it by reading their own scrape config.
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// Check whether the request can read /v1/metrics.
///
/// `expected` is the configured token, `None` if metrics auth is disabled
/// entirely. Disabled means "endpoint not exposed" — we deliberately return
/// false here so the handler 404s. There is no "open metrics" mode by design;
/// see Codex finding #1 for why.
///
/// Comparison is constant-time-ish via `subtle`-style byte-by-byte equality
/// to make timing-attack pre-image search uninteresting; for a 32+ byte
/// random token this is theoretical at best, but it costs nothing.
pub(crate) fn metrics_auth_ok(
    expected: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

use crate::state::AppState;

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    components: Vec<ComponentHealth>,
}

#[derive(Serialize)]
struct ComponentHealth {
    name: &'static str,
    ok: bool,
}

/// Cheap liveness probe. Returns 200 immediately as long as the HTTP layer
/// is responsive. Does NOT touch MySQL/Milvus — those are checked by
/// /v1/ready (readiness). systemd watchdog / k8s livenessProbe should hit
/// this endpoint, not /v1/ready, so a transient DB blip doesn't trigger
/// process restarts that won't help.
async fn healthz() -> &'static str {
    "ok"
}

/// Public, unauthenticated capability probe so clients (FUSE in
/// particular) can decide whether to advertise summary sidecars
/// without paying for a per-directory 501 round-trip. Currently
/// reports a single bit (`summary_enabled`) that mirrors
/// `AppState::summary_enabled`. Extend with additional flags
/// when new optional features ship — keep the shape backwards-
/// compatible so old clients ignore unknown keys.
///
/// Mounted at `/capabilities` (NOT `/v1/capabilities`) so a hardened
/// reverse proxy that enforces auth on the entire `/v1/*` namespace
/// still lets the probe through — same reasoning as `/healthz`. Without
/// this, a 401 from the proxy would let FUSE silently fall back to
/// "assume summary_enabled=true" and surface phantom sidecars.
async fn capabilities(State(state): State<Arc<AppState>>) -> Response {
    Json(capabilities_payload(state.summary_enabled)).into_response()
}

/// Wire-shape payload for [`capabilities`]. Split out so a unit
/// test can pin `data.summary_enabled` without standing up
/// `AppState` — the FUSE client deserialises this exact shape, so
/// a silent rename or wrapper change here would break the probe
/// path with no compile-time signal.
fn capabilities_payload(summary_enabled: bool) -> ApiResponse<serde_json::Value> {
    ApiResponse::ok(serde_json::json!({
        "summary_enabled": summary_enabled,
    }))
}

/// Identity probe for the data plane: resolve the presented `wk_` to
/// the workspace it belongs to. Accepts keys of either kind (fs/db) —
/// a pasted `wk_` carries no workspace id, so clients (CLI `status` /
/// `init --import-key`) call this to backfill their local config.
async fn whoami(auth: crate::auth::AuthAnyWorkspace) -> Response {
    Json(whoami_payload(&auth)).into_response()
}

/// Wire-shape payload for [`whoami`]. Split out so a unit test can pin
/// the field names — the CLI deserialises this exact shape to backfill
/// `workspace.id` in its config, so a silent rename would break the
/// backfill with no compile-time signal.
fn whoami_payload(auth: &crate::auth::AuthAnyWorkspace) -> ApiResponse<serde_json::Value> {
    ApiResponse::ok(serde_json::json!({
        "workspace_id": auth.workspace_id,
        "kind": auth.kind,
        "permission": auth.permission,
    }))
}

async fn install_script() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
        ],
        INSTALL_SH,
    )
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    // Drain window: SIGTERM received, still serving traffic. Report 503
    // without pinging dependencies so the LB health check flips fast and
    // pulls this node before the listener closes.
    if state.draining.load(std::sync::atomic::Ordering::Relaxed) {
        let body = ReadyResponse {
            status: "draining",
            components: vec![],
        };
        return (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
    }
    let (mysql_res, milvus_res) = tokio::join!(
        tokio::time::timeout(READY_TIMEOUT, state.meta_store.ping()),
        tokio::time::timeout(READY_TIMEOUT, state.vector_store.ping()),
    );
    let mysql_ok = mysql_res.map(|r| r.is_ok()).unwrap_or(false);
    let milvus_ok = milvus_res.map(|r| r.is_ok()).unwrap_or(false);
    let all_ok = mysql_ok && milvus_ok;
    let body = ReadyResponse {
        status: if all_ok { "ready" } else { "not_ready" },
        components: vec![
            ComponentHealth {
                name: "mysql",
                ok: mysql_ok,
            },
            ComponentHealth {
                name: "milvus",
                ok: milvus_ok,
            },
        ],
    };
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_response_serializes_correctly() {
        let resp = ReadyResponse {
            status: "ready",
            components: vec![
                ComponentHealth {
                    name: "mysql",
                    ok: true,
                },
                ComponentHealth {
                    name: "milvus",
                    ok: true,
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["components"][0]["name"], "mysql");
        assert_eq!(json["components"][0]["ok"], true);
        assert_eq!(json["components"][1]["name"], "milvus");
    }

    #[test]
    fn capabilities_payload_reports_summary_enabled_true() {
        // The FUSE client (`crates/veda-fuse/src/client.rs::get_capabilities`)
        // mocks this exact shape in its unit tests. Pin it here so
        // an accidental rename ("summaries_enabled") or envelope
        // change (ApiResponse::err) fails CI on the server side
        // before it can quietly break the FUSE probe.
        let resp = capabilities_payload(true);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["summary_enabled"], true);
        // Documented field name — the FUSE Capabilities struct
        // uses serde(default) so a typo would deserialise as false.
        assert!(json["data"].get("summary_enabled").is_some());
    }

    #[test]
    fn capabilities_payload_reports_summary_enabled_false() {
        let resp = capabilities_payload(false);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["summary_enabled"], false);
    }

    #[test]
    fn whoami_payload_pins_wire_shape() {
        // The CLI (`veda-cli::init::backfill_active_workspace_id`) reads
        // data.workspace_id from this exact shape — pin the field names
        // and enum spellings so a rename fails here, not in the field.
        let auth = crate::auth::AuthAnyWorkspace {
            workspace_id: "ws-123".into(),
            kind: veda_types::WorkspaceKind::Fs,
            permission: veda_types::KeyPermission::ReadWrite,
        };
        let json = serde_json::to_value(whoami_payload(&auth)).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["workspace_id"], "ws-123");
        assert_eq!(json["data"]["kind"], "fs");
        assert_eq!(json["data"]["permission"], "readwrite");
    }

    #[test]
    fn ready_response_not_ready() {
        let resp = ReadyResponse {
            status: "not_ready",
            components: vec![
                ComponentHealth {
                    name: "mysql",
                    ok: true,
                },
                ComponentHealth {
                    name: "milvus",
                    ok: false,
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "not_ready");
        assert_eq!(json["components"][1]["ok"], false);
    }

    fn hdr_with_auth(value: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn metrics_auth_disabled_when_token_unset() {
        let h = hdr_with_auth("Bearer anything");
        assert!(!metrics_auth_ok(None, &h));
    }

    #[test]
    fn metrics_auth_disabled_when_token_empty_string() {
        // Explicitly-empty token shouldn't accidentally allow empty bearer.
        let h = hdr_with_auth("Bearer ");
        assert!(!metrics_auth_ok(Some(""), &h));
    }

    #[test]
    fn metrics_auth_rejects_missing_authorization_header() {
        let h = axum::http::HeaderMap::new();
        assert!(!metrics_auth_ok(Some("real-token"), &h));
    }

    #[test]
    fn metrics_auth_rejects_wrong_scheme() {
        let h = hdr_with_auth("Basic real-token");
        assert!(!metrics_auth_ok(Some("real-token"), &h));
    }

    #[test]
    fn metrics_auth_rejects_wrong_token() {
        let h = hdr_with_auth("Bearer wrong-token");
        assert!(!metrics_auth_ok(Some("real-token"), &h));
    }

    #[test]
    fn metrics_auth_accepts_correct_token() {
        let h = hdr_with_auth("Bearer real-token");
        assert!(metrics_auth_ok(Some("real-token"), &h));
    }

    #[test]
    fn constant_time_eq_handles_length_difference() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}

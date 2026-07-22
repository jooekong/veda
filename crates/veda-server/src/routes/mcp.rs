//! MCP (Model Context Protocol) endpoint — `POST /mcp`.
//!
//! Streamable HTTP transport in **stateless** mode: every request is an
//! independent JSON-RPC message authenticated by `Authorization: Bearer wk_…`
//! (the same `AuthWorkspace` gate as the REST data plane, fs-kind only).
//! No session ids, no server-initiated SSE stream — a plain JSON response
//! per POST. GET/DELETE on `/mcp` get axum's automatic 405, which per spec
//! tells clients "no downstream stream / no client-terminated sessions".
//!
//! Six read-only tools, all thin wrappers over the in-process service layer
//! (never the HTTP loopback): search / grep / read_file / list_dir /
//! overview / ask. `ask` shares the per-workspace concurrency gate and
//! metrics histograms with `POST /v1/answer` (routes/answer.rs) so both
//! surfaces draw from one LLM budget.
//!
//! Error split:
//! - protocol errors (bad JSON, unknown method/tool, invalid params) →
//!   JSON-RPC `error` objects;
//! - domain errors (file not found, feature disabled, throttled, timeout) →
//!   `result.isError = true` with a readable message, so the calling LLM can
//!   see what happened and self-correct — same philosophy as the answer
//!   loop's tool-error backfill.
//!
//! No Origin validation (deliberate): the MCP spec's DNS-rebinding guidance
//! targets unauthenticated localhost servers. This endpoint requires a
//! `wk_` bearer on every request — a rebinding page in a browser cannot
//! attach that header, so there is nothing for it to reach. Revisit only if
//! an unauthenticated surface is ever added here.
//!
//! Design: docs/plans/coding-agent-kb-plan.md §4.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing::{error, info, warn};
use veda_core::service::answer::AnswerError;
use veda_types::{DetailLevel, SearchMode, VedaError};

use crate::auth::AuthWorkspace;
use crate::routes::answer::{
    answer_outcome_label, gate_for, record_answer_stats, valid_query, AnswerReqTimer,
};
use crate::state::AppState;

/// Protocol revisions this server speaks. `initialize` echoes the client's
/// requested version when it is one of these, otherwise offers the first
/// entry (clients then disconnect if they can't work with it).
///
/// 2025-06-18 only: the 2025-03-26 revision REQUIRES JSON-RPC batch support
/// on this transport, which a stateless single-message server deliberately
/// doesn't implement — advertising it would be a lie (codex review 07-22).
const PROTOCOL_VERSIONS: [&str; 1] = ["2025-06-18"];

/// Per-call wall clock for every tool except `ask`. These tools used to sit
/// under the router-wide 30s TimeoutLayer on the REST surface; `/mcp` is
/// mounted outside that layer (because of `ask`), so the budget is enforced
/// here instead.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// `ask` budget: the answer service's route-level deadline is 90s
/// (routes/answer.rs::ANSWER_DEADLINE); +5s headroom so the inner timeout —
/// with its precise error message — fires first.
const ASK_TOOL_TIMEOUT: Duration = Duration::from_secs(95);

/// Same deadline as `POST /v1/answer` wraps around the agentic loop.
const ASK_DEADLINE: Duration = Duration::from_secs(90);

/// `ask` pre-search candidate count — the /v1/answer default (route caps at
/// 24; the tool doesn't expose the knob, fewer knobs for the calling LLM).
const ASK_LIMIT: usize = 12;

/// Whole-file reads are capped so one tool call can't flood the calling
/// agent's context window; the truncation note steers it to line paging.
const READ_CAP_BYTES: usize = 64 * 1024;

/// Default page height when `start_line` is given without `end_line`.
const READ_PAGE_LINES: i64 = 500;

/// Recursive listing cap — MCP consumers browse, they don't mirror. The
/// service-side ceiling (MAX_RECURSIVE_DESCENT = 100k) stays the backstop.
const LIST_RECURSIVE_CAP: usize = 10_000;

/// grep hit cap (REST default is 100; a browsing agent may legitimately want
/// more, but unbounded scans are the server's problem, not the tool's).
const GREP_CAP: u64 = 1000;

const SEARCH_CAP: u64 = 100;

/// grep is a locator, not a reader: each matched line is clipped to this
/// many bytes so a minified/single-line file can't turn 1000 hits into a
/// multi-hundred-MB response.
const GREP_LINE_CAP: usize = 500;

/// Flat (non-recursive) listings are clipped to the same ceiling as
/// recursive ones; the service loads the full child list either way (same
/// as REST list), this only bounds the response we ship back.
const LIST_FLAT_CAP: usize = 10_000;

pub fn routes() -> Router<Arc<AppState>> {
    // POST only. axum answers GET/DELETE with 405 + Allow automatically.
    Router::new().route("/mcp", post(mcp_post))
}

// ── JSON-RPC plumbing ──────────────────────────────────

/// Protocol-level failure → JSON-RPC error object.
#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }
}

/// Tool execution outcome that is NOT a clean success.
#[derive(Debug)]
enum ToolError {
    /// Caller broke the contract (unknown tool, missing/bad argument).
    Rpc(RpcError),
    /// Domain failure the calling LLM should see and react to.
    Domain(String),
}

impl From<RpcError> for ToolError {
    fn from(e: RpcError) -> Self {
        ToolError::Rpc(e)
    }
}

/// Strict JSON-RPC 2.0 request-object validation. Returns the parsed
/// (method, id, params) triple, `Ok(None)` for a well-formed notification
/// (no `id` member at all), or the ready-made error response.
///
/// Everything that fails validation gets `-32600` with `id: null` — per the
/// JSON-RPC spec, when the request object is invalid the id cannot be
/// trusted, so it is not echoed.
fn validate_request(msg: &Value) -> Result<Option<(String, Value, Value)>, Response> {
    // 2025-06-18 dropped JSON-RPC batching; this stateless server never
    // advertises a batch-capable revision (see PROTOCOL_VERSIONS).
    if msg.is_array() {
        return Err(rpc_error_response(
            Value::Null,
            -32600,
            "batch requests are not supported",
        ));
    }
    if !msg.is_object() {
        return Err(rpc_error_response(
            Value::Null,
            -32600,
            "invalid request: not a JSON-RPC object",
        ));
    }
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(rpc_error_response(
            Value::Null,
            -32600,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }
    let id = match msg.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(v.clone()),
        Some(_) => {
            return Err(rpc_error_response(
                Value::Null,
                -32600,
                "invalid request: id must be a string, number or null",
            ));
        }
    };
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return Err(rpc_error_response(
            Value::Null,
            -32600,
            "invalid request: missing method",
        ));
    };
    let params = match msg.get("params") {
        None | Some(Value::Null) => Value::Null,
        // MCP methods all take named params; positional (array) params
        // don't exist on this surface.
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => {
            return Err(rpc_error_response(
                Value::Null,
                -32600,
                "invalid request: params must be an object",
            ));
        }
    };
    match id {
        // Notification: no `id` member. (`"id": null` is a request whose
        // response carries id null — distinct per spec.)
        None => Ok(None),
        Some(id) => Ok(Some((method.to_string(), id, params))),
    }
}

/// 2025-06-18 requires clients to send `MCP-Protocol-Version` on every
/// request after initialize, and servers to 400 unsupported values. Absent
/// header = pre-negotiation or older client → allowed (the message shapes
/// this server consumes are identical across revisions).
fn protocol_version_rejected(headers: &axum::http::HeaderMap) -> Option<Response> {
    let raw = headers.get("mcp-protocol-version")?;
    // Unparsable header bytes count as unsupported, not as absent.
    let ok = raw
        .to_str()
        .map(|v| PROTOCOL_VERSIONS.contains(&v))
        .unwrap_or(false);
    if ok {
        None
    } else {
        Some(
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "unsupported MCP-Protocol-Version (supported: {})",
                    PROTOCOL_VERSIONS.join(", ")
                ),
            )
                .into_response(),
        )
    }
}

async fn mcp_post(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();

    if let Some(reject) = protocol_version_rejected(&headers) {
        return reject;
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error_response(Value::Null, -32700, "parse error: body is not valid JSON");
    };
    let (method, id, params) = match validate_request(&msg) {
        Err(resp) => return resp,
        // Notifications expect no response body: 202 per streamable-http.
        Ok(None) => return StatusCode::ACCEPTED.into_response(),
        Ok(Some(triple)) => triple,
    };

    // Metric labels come only from fixed strings / the tool whitelist —
    // request-supplied method or tool names must never become label values,
    // or arbitrary clients could explode the metrics cardinality.
    let (label, result): (&'static str, Result<Value, RpcError>) = match method.as_str() {
        "initialize" => ("initialize", Ok(initialize_result(&params))),
        "ping" => ("ping", Ok(json!({}))),
        "tools/list" => ("tools/list", Ok(json!({ "tools": tool_specs() }))),
        "tools/call" => {
            let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
            let label = tool_metric_label(tool);
            (label, tools_call(&state, &auth, &params).await)
        }
        other => ("unknown", Err(RpcError::method_not_found(other))),
    };

    match result {
        Ok(v) => {
            record_mcp(label, "ok", started);
            rpc_ok_response(id, v)
        }
        Err(e) => {
            record_mcp(label, "error", started);
            rpc_error_response(id, e.code, &e.message)
        }
    }
}

fn rpc_ok_response(id: Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// JSON-RPC errors ride HTTP 200 — the transport delivered fine, the failure
/// is at the protocol layer. Widest client compatibility.
fn rpc_error_response(id: Value, code: i64, message: &str) -> Response {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }))
    .into_response()
}

fn record_mcp(method: &'static str, outcome: &'static str, started: Instant) {
    ::metrics::histogram!(
        "veda_mcp_request_seconds",
        "method" => method,
        "outcome" => outcome,
    )
    .record(started.elapsed().as_secs_f64());
}

/// Whitelisted metric label for a tools/call — never the raw client string.
fn tool_metric_label(tool: &str) -> &'static str {
    match tool {
        "search" => "tool:search",
        "grep" => "tool:grep",
        "read_file" => "tool:read_file",
        "list_dir" => "tool:list_dir",
        "overview" => "tool:overview",
        "ask" => "tool:ask",
        _ => "tool:unknown",
    }
}

// ── initialize / tools list ────────────────────────────

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "veda", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Read-only access to a veda knowledge workspace. \
            Start with `search` (detail_level='abstract' scans relevance at ~100 tokens/hit), \
            then `read_file` the promising paths. `grep` finds exact strings with line numbers. \
            `ask` returns a complete answer with [n] citations for open questions."
    })
}

/// Tool catalogue. Descriptions are written for the calling LLM: they carry
/// the token-economics guidance (L0 → L2 escalation) that makes the tiered
/// model useful, and they spell out sharp edges (literal grep, truncation).
fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Hybrid semantic + keyword (BM25) search over the knowledge base. \
                Returns matching chunks with file path, score and content. \
                TIP: set detail_level='abstract' first — each hit then carries a ~100-token \
                file summary instead of raw chunks, which is the cheap way to find out what is \
                relevant before reading files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language or keyword query." },
                    "limit": { "type": "integer", "description": "Max hits, default 10, cap 100." },
                    "path_prefix": { "type": "string", "description": "Restrict to a subtree, e.g. '/wiki/backend'." },
                    "detail_level": { "type": "string", "enum": ["abstract", "overview", "full"],
                        "description": "full = raw chunks (default); abstract = L0 one-liner per file; overview = L1 structured summary." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "grep",
            "description": "Literal substring scan (NOT a regex) across all text files. \
                Returns path, 1-indexed line number and the matching line — the only tool that \
                gives exact line positions. Use for identifiers, error codes, exact phrases.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Literal substring to find." },
                    "path": { "type": "string", "description": "Restrict to a path prefix." },
                    "ignore_case": { "type": "boolean", "description": "Case-insensitive match, default false." },
                    "limit": { "type": "integer", "description": "Max hits, default 100, cap 1000." }
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "read_file",
            "description": "Read a file's text content. PDF and Word files return their extracted \
                text. Whole-file reads over 64KB are truncated — page through big files with \
                start_line/end_line instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute file path, e.g. '/wiki/arch.md'." },
                    "start_line": { "type": "integer", "description": "1-indexed first line. Omit to read the whole file." },
                    "end_line": { "type": "integer", "description": "Last line inclusive; defaults to start_line+499." }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "list_dir",
            "description": "List a directory. recursive=true walks the whole subtree \
                (paths only, capped at 10000 entries).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path, default '/'." },
                    "recursive": { "type": "boolean", "description": "Walk the subtree, default false." }
                }
            }
        }),
        json!({
            "name": "overview",
            "description": "Structured L1 overview (~2k tokens) of one file or directory — richer \
                than a search snippet, far cheaper than reading a large file. Generated \
                asynchronously after upload, so very fresh paths may report 'not ready yet'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory path." }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "ask",
            "description": "One-shot RAG answer: the server searches the knowledge base itself and \
                answers with inline [n] citations plus the source paths. Use for open or \
                multi-document questions when you want a synthesized answer rather than raw \
                chunks. May take 10-90 seconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, max 1024 chars." },
                    "path_prefix": { "type": "string", "description": "Restrict retrieval to a subtree." }
                },
                "required": ["question"]
            }
        }),
    ]
}

// ── tools/call dispatch ────────────────────────────────

async fn tools_call(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    params: &Value,
) -> Result<Value, RpcError> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(RpcError::invalid_params("missing tool name"));
    };
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let timeout = if name == "ask" {
        ASK_TOOL_TIMEOUT
    } else {
        TOOL_TIMEOUT
    };
    match tokio::time::timeout(timeout, run_tool(state, auth, name, &args)).await {
        Ok(Ok(text)) => Ok(tool_result(text, false)),
        Ok(Err(ToolError::Rpc(e))) => Err(e),
        Ok(Err(ToolError::Domain(text))) => Ok(tool_result(text, true)),
        Err(_elapsed) => Ok(tool_result(format!("tool '{name}' timed out"), true)),
    }
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

async fn run_tool(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    name: &str,
    args: &Value,
) -> Result<String, ToolError> {
    match name {
        "search" => tool_search(state, auth, args).await,
        "grep" => tool_grep(state, auth, args).await,
        "read_file" => tool_read_file(state, auth, args).await,
        "list_dir" => tool_list_dir(state, auth, args).await,
        "overview" => tool_overview(state, auth, args).await,
        "ask" => tool_ask(state, auth, args).await,
        other => Err(RpcError::invalid_params(format!("unknown tool: {other}")).into()),
    }
}

// ── argument helpers ───────────────────────────────────

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params(format!("missing required string argument: {key}")).into()
        })
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Users (and LLMs) pass both "/wiki/a.md" and "wiki/a.md"; the service layer
/// expects a leading slash.
fn lead_slash(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Map a service error to tool-result text. Internal-class errors collapse to
/// an opaque message (full detail goes to the log) — same policy as
/// `error.rs::AppError` so the MCP surface can't leak storage internals.
fn safe_error_text(e: &VedaError) -> String {
    match e {
        VedaError::EmbeddingFailed(_)
        | VedaError::Deadlock(_)
        | VedaError::Storage(_)
        | VedaError::Internal(_) => {
            error!(err = %e, "mcp tool internal error");
            "internal server error".into()
        }
        other => other.to_string(),
    }
}

fn domain(e: VedaError) -> ToolError {
    ToolError::Domain(safe_error_text(&e))
}

/// Serialize a tool payload. Failure is a server bug on these plain
/// structs — log it, keep the wire text opaque (same policy as VedaError
/// internal-class errors).
fn to_json_text<T: serde::Serialize>(v: &T) -> Result<String, ToolError> {
    serde_json::to_string(v).map_err(|e| {
        error!(err = %e, "mcp tool result serialization failed");
        ToolError::Domain("internal server error".into())
    })
}

// ── the six tools ──────────────────────────────────────

async fn tool_search(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let query = required_str(args, "query")?;
    let limit = opt_u64(args, "limit").unwrap_or(10).min(SEARCH_CAP) as usize;
    let path_prefix = opt_str(args, "path_prefix").map(lead_slash);
    let detail_level = match opt_str(args, "detail_level") {
        None => DetailLevel::Full,
        Some(s) => serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
            RpcError::invalid_params("detail_level must be one of: abstract, overview, full")
        })?,
    };

    let hits = state
        .search_service
        .search(
            &auth.workspace_id,
            query,
            SearchMode::Hybrid,
            limit,
            path_prefix.as_deref(),
            detail_level,
        )
        .await
        .map_err(domain)?;
    // SearchHit's Serialize already strips server-side fields (file_id).
    to_json_text(&hits)
}

async fn tool_grep(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let pattern = required_str(args, "pattern")?;
    let path_prefix = opt_str(args, "path").map(lead_slash);
    let ignore_case = opt_bool(args, "ignore_case");
    let limit = opt_u64(args, "limit").unwrap_or(100).min(GREP_CAP) as usize;

    let mut hits = state
        .fs_service
        .grep(
            &auth.workspace_id,
            pattern,
            path_prefix.as_deref(),
            ignore_case,
            limit,
        )
        .await
        .map_err(domain)?;
    // grep locates, read_file reads: clip each matched line so a minified /
    // single-line file can't blow the response up to hits × megabytes.
    for h in &mut hits {
        if h.line.len() > GREP_LINE_CAP {
            let (clipped, _) = truncate_utf8(std::mem::take(&mut h.line), GREP_LINE_CAP);
            h.line = format!("{clipped}…");
        }
    }
    to_json_text(&hits)
}

async fn tool_read_file(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let path = lead_slash(required_str(args, "path")?);
    let start_line = args.get("start_line").and_then(Value::as_i64);
    let end_line = args.get("end_line").and_then(Value::as_i64);

    match (start_line, end_line) {
        (None, None) => {
            // Whole file; pdf/word blobs come back as their extracted text.
            let text = state
                .fs_service
                .read_file(&auth.workspace_id, &path)
                .await
                .map_err(domain)?;
            let total = text.len();
            let (body, truncated) = truncate_utf8(text, READ_CAP_BYTES);
            Ok(if truncated {
                format!(
                    "{body}\n\n[truncated: file is {total} bytes; \
                     use start_line/end_line to read further]"
                )
            } else {
                body
            })
        }
        (Some(start), end) => {
            if start < 1 {
                return Err(RpcError::invalid_params("start_line must be >= 1").into());
            }
            let end = end.unwrap_or(start.saturating_add(READ_PAGE_LINES - 1));
            if end < start {
                return Err(RpcError::invalid_params("end_line must be >= start_line").into());
            }
            let start = i32::try_from(start)
                .map_err(|_| RpcError::invalid_params("start_line out of range"))?;
            let end = i32::try_from(end.min(i32::MAX as i64)).unwrap_or(i32::MAX);
            let text = state
                .fs_service
                .read_file_lines(&auth.workspace_id, &path, start, end)
                .await
                .map_err(domain)?;
            // The byte cap applies to line ranges too — a 50MB single-line
            // file must not ride out through `start_line=1,end_line=1`.
            let total = text.len();
            let (body, truncated) = truncate_utf8(text, READ_CAP_BYTES);
            Ok(if truncated {
                format!("{body}\n\n[truncated: this line range is {total} bytes]")
            } else {
                body
            })
        }
        (None, Some(_)) => {
            Err(RpcError::invalid_params("end_line requires start_line").into())
        }
    }
}

async fn tool_list_dir(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let path = lead_slash(opt_str(args, "path").unwrap_or("/"));
    if opt_bool(args, "recursive") {
        // The service errors with QuotaExceeded above the cap instead of
        // clipping — so a successful return is always the COMPLETE subtree
        // (truncated: false is a fact, not an assumption).
        let dentries = match state
            .fs_service
            .list_dir_recursive(&auth.workspace_id, &path, LIST_RECURSIVE_CAP)
            .await
        {
            Ok(d) => d,
            Err(VedaError::QuotaExceeded(_)) => {
                return Err(ToolError::Domain(format!(
                    "subtree under {path} has more than {LIST_RECURSIVE_CAP} entries; \
                     list a deeper path, or use search/grep to locate files instead"
                )));
            }
            Err(e) => return Err(domain(e)),
        };
        let entries: Vec<Value> = dentries
            .into_iter()
            .map(|d| json!({ "path": d.path, "is_dir": d.is_dir }))
            .collect();
        to_json_text(&json!({ "entries": entries, "truncated": false }))
    } else {
        let mut entries = state
            .fs_service
            .list_dir(&auth.workspace_id, &path)
            .await
            .map_err(domain)?;
        // Flat listings have no service-side ceiling; bound the response.
        let truncated = entries.len() > LIST_FLAT_CAP;
        entries.truncate(LIST_FLAT_CAP);
        to_json_text(&json!({ "entries": entries, "truncated": truncated }))
    }
}

async fn tool_overview(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let path = lead_slash(required_str(args, "path")?);
    let summary = state
        .search_service
        .get_summary(&auth.workspace_id, &path)
        .await
        .map_err(domain)?;
    match summary {
        Some(s) => Ok(json!({ "path": path, "l1_overview": s.l1_overview }).to_string()),
        // isError=true in both arms: "pending" invites a retry, "disabled"
        // tells the agent to stop asking — mirrors the 202/501 REST split.
        None if state.summary_enabled => Err(ToolError::Domain(
            "overview not ready yet — summaries are generated asynchronously after upload; \
             retry in a few seconds or use search/read_file instead"
                .into(),
        )),
        None => Err(ToolError::Domain(
            "summaries are disabled on this server (no LLM configured); \
             use search or read_file instead"
                .into(),
        )),
    }
}

async fn tool_ask(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    args: &Value,
) -> Result<String, ToolError> {
    let question = required_str(args, "question")?.trim();
    if !valid_query(question) {
        return Err(
            RpcError::invalid_params("question must be non-empty and at most 1024 characters")
                .into(),
        );
    }
    let path_prefix = opt_str(args, "path_prefix").map(lead_slash);

    let Some(svc) = state.answer_service.clone() else {
        return Err(ToolError::Domain(
            "ask is disabled on this server (no LLM configured); use search instead".into(),
        ));
    };

    // Same per-workspace gate as POST /v1/answer: one LLM concurrency budget
    // across both surfaces.
    let mut timer = AnswerReqTimer::start(&auth.workspace_id);
    let gate = gate_for(&auth.workspace_id, state.answer_concurrency);
    let Ok(_permit) = gate.try_acquire_owned() else {
        timer.set_outcome("throttled");
        return Err(ToolError::Domain(
            "too many concurrent ask/answer requests for this workspace; retry shortly".into(),
        ));
    };

    let outcome = tokio::time::timeout(
        ASK_DEADLINE,
        svc.answer(
            &auth.workspace_id,
            question,
            path_prefix.as_deref(),
            ASK_LIMIT,
            None,
        ),
    )
    .await;

    match outcome {
        Err(_elapsed) => {
            timer.set_outcome("timeout");
            Err(ToolError::Domain(
                "answer generation exceeded the deadline".into(),
            ))
        }
        Ok(Ok(r)) => {
            timer.set_outcome(answer_outcome_label(r.grounded, &r.answer));
            record_answer_stats(
                &auth.workspace_id,
                r.hit_count,
                r.estimated_context_tokens,
                r.rounds,
            );
            info!(
                hit_count = r.hit_count,
                grounded = r.grounded,
                rounds = r.rounds,
                surface = "mcp",
                "answer produced"
            );
            Ok(json!({
                "answer": r.answer,
                "citations": r.citations,
                "hit_count": r.hit_count,
            })
            .to_string())
        }
        Ok(Err(AnswerError::LlmFailed(e))) => {
            timer.set_outcome("llm_error");
            warn!(err = %e, "mcp ask: llm failed");
            Err(ToolError::Domain("LLM upstream unavailable".into()))
        }
        Ok(Err(AnswerError::Timeout)) => {
            timer.set_outcome("timeout");
            Err(ToolError::Domain(
                "answer generation exceeded the deadline".into(),
            ))
        }
        Ok(Err(AnswerError::Store(e))) => Err(domain(e)),
    }
}

/// Cut `s` down to at most `cap` bytes on a char boundary.
fn truncate_utf8(s: String, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_supported_version() {
        let r = initialize_result(&json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(r["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn initialize_falls_back_on_unsupported_version() {
        // 2025-03-26 is deliberately NOT advertised (it would require batch
        // support) — the server counter-offers its newest revision.
        let r = initialize_result(&json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(r["protocolVersion"], "2025-06-18");
        let r = initialize_result(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSIONS[0]);
        let r = initialize_result(&Value::Null);
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn validate_request_strictness() {
        // Well-formed request.
        let ok = validate_request(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        let (m, id, _) = ok.unwrap().unwrap();
        assert_eq!(m, "ping");
        assert_eq!(id, json!(1));
        // Notification: no id member at all.
        assert!(validate_request(&json!({"jsonrpc":"2.0","method":"x"}))
            .unwrap()
            .is_none());
        // "id": null is a REQUEST (responded to with id null), not a notification.
        let (_, id, _) = validate_request(&json!({"jsonrpc":"2.0","id":null,"method":"ping"}))
            .unwrap()
            .unwrap();
        assert_eq!(id, Value::Null);
        // Rejections: bad jsonrpc / object id / array params / non-object.
        assert!(validate_request(&json!({"id":1,"method":"ping"})).is_err());
        assert!(validate_request(&json!({"jsonrpc":"1.0","id":1,"method":"ping"})).is_err());
        assert!(
            validate_request(&json!({"jsonrpc":"2.0","id":{},"method":"ping"})).is_err(),
            "object id must be rejected"
        );
        assert!(
            validate_request(&json!({"jsonrpc":"2.0","id":[1],"method":"ping"})).is_err(),
            "array id must be rejected"
        );
        assert!(
            validate_request(&json!({"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}))
                .is_err(),
            "positional params must be rejected"
        );
        assert!(validate_request(&json!([{"jsonrpc":"2.0","id":1,"method":"ping"}])).is_err());
        assert!(validate_request(&json!("nope")).is_err());
    }

    #[test]
    fn protocol_version_header_gate() {
        let mut h = axum::http::HeaderMap::new();
        assert!(protocol_version_rejected(&h).is_none(), "absent header allowed");
        h.insert("mcp-protocol-version", "2025-06-18".parse().unwrap());
        assert!(protocol_version_rejected(&h).is_none(), "supported version allowed");
        h.insert("mcp-protocol-version", "2025-03-26".parse().unwrap());
        assert!(
            protocol_version_rejected(&h).is_some(),
            "unadvertised revision → 400"
        );
    }

    #[test]
    fn initialize_advertises_tools_capability_and_identity() {
        let r = initialize_result(&Value::Null);
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["serverInfo"]["name"], "veda");
        assert_eq!(r["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(r["instructions"].as_str().unwrap().contains("search"));
    }

    #[test]
    fn tool_specs_lists_six_valid_tools() {
        let specs = tool_specs();
        let names: Vec<&str> = specs
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["search", "grep", "read_file", "list_dir", "overview", "ask"]
        );
        for t in &specs {
            assert!(
                !t["description"].as_str().unwrap().is_empty(),
                "{} has empty description",
                t["name"]
            );
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
        // Required fields spelled correctly — a typo here surfaces as LLMs
        // omitting the argument at call time, which is painful to debug.
        assert_eq!(specs[0]["inputSchema"]["required"][0], "query");
        assert_eq!(specs[1]["inputSchema"]["required"][0], "pattern");
        assert_eq!(specs[2]["inputSchema"]["required"][0], "path");
        assert_eq!(specs[4]["inputSchema"]["required"][0], "path");
        assert_eq!(specs[5]["inputSchema"]["required"][0], "question");
    }

    #[test]
    fn truncate_utf8_respects_char_boundaries() {
        // "中" is 3 bytes; a cap that lands mid-char must back off.
        let s = "中中中".to_string(); // 9 bytes
        let (out, truncated) = truncate_utf8(s.clone(), 4);
        assert_eq!(out, "中");
        assert!(truncated);
        let (out, truncated) = truncate_utf8(s, 9);
        assert_eq!(out, "中中中");
        assert!(!truncated);
    }

    #[test]
    fn lead_slash_normalizes() {
        assert_eq!(lead_slash("wiki/a.md"), "/wiki/a.md");
        assert_eq!(lead_slash("/wiki/a.md"), "/wiki/a.md");
    }

    #[test]
    fn required_str_rejects_missing_and_empty() {
        assert!(required_str(&json!({}), "query").is_err());
        assert!(required_str(&json!({ "query": "" }), "query").is_err());
        assert!(required_str(&json!({ "query": 42 }), "query").is_err());
        assert_eq!(required_str(&json!({ "query": "x" }), "query").unwrap(), "x");
    }

    #[test]
    fn tool_result_shape() {
        let v = tool_result("hello".into(), false);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
        assert_eq!(v["isError"], false);
        let v = tool_result("boom".into(), true);
        assert_eq!(v["isError"], true);
    }

    #[test]
    fn safe_error_text_hides_internal_detail() {
        let e = VedaError::Storage("sqlx: SELECT secret FROM t".into());
        assert_eq!(safe_error_text(&e), "internal server error");
        let e = VedaError::NotFound("file not found: /a.md".into());
        assert!(safe_error_text(&e).contains("/a.md"));
    }
}

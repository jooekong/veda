//! Agentic RAG answer endpoint (`POST /v1/answer`): the LLM drives retrieval
//! through tool calls and answers with verifiable citations. This is a thin
//! route shell — auth, DTO validation, the per-workspace concurrency gate,
//! the total deadline, and error/metrics mapping. The loop lives in
//! `veda_core::service::answer::AnswerService`.
//!
//! fs-kind only: the `AuthWorkspace` extractor enforces `kind == Fs` and
//! returns `WORKSPACE_KIND_MISMATCH` (400) for a db `wk_`. See
//! `docs/plans/veda-answer-agentic.md`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::Semaphore;
use tracing::{info, warn};
use veda_core::service::answer::{AnswerError, AnswerStreamEvent, NO_CONTEXT_ANSWER};
use veda_types::api::{AnswerApiRequest, AnswerApiResponse};
use veda_types::ApiResponse;

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

/// Query length ceiling (chars, post-trim).
const MAX_QUERY_CHARS: usize = 1024;
/// Custom bot prompt ceiling (chars). Long enough for a rich persona, short
/// enough to keep the system prompt bounded.
const MAX_PROMPT_CHARS: usize = 4000;
/// Initial pre-search candidate count: default 12, capped at 24.
const DEFAULT_LIMIT: usize = 12;
const MAX_LIMIT: usize = 24;
/// Route-level backstop around the whole agentic loop (whose own budget is
/// AnswerParams::total_budget = 80s). The route is mounted OUTSIDE the 30s
/// TimeoutLayer (see `routes/mod.rs`) precisely so this longer deadline
/// applies.
const ANSWER_DEADLINE: Duration = Duration::from_secs(90);

/// Per-workspace answer concurrency gates. Process-local (single pod is enough
/// per the simplification convention). A semaphore is created lazily per
/// workspace with `AppState::answer_concurrency` permits, then reused.
static GATES: LazyLock<Mutex<HashMap<String, Arc<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fetch (or lazily create) the concurrency semaphore for a workspace. The lock
/// is held only for the map lookup — never across an await.
///
/// pub(crate): the MCP `ask` tool (routes/mcp.rs) funnels through the SAME
/// per-workspace gate — REST and MCP answers share one concurrency budget,
/// otherwise a workspace could double its LLM spend by mixing surfaces.
pub(crate) fn gate_for(workspace_id: &str, permits: usize) -> Arc<Semaphore> {
    let mut gates = GATES.lock().unwrap();
    gates
        .entry(workspace_id.to_string())
        // `.max(1)` so a misconfigured concurrency=0 doesn't wedge a workspace
        // into permanent 429s.
        .or_insert_with(|| Arc::new(Semaphore::new(permits.max(1))))
        .clone()
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/answer", post(answer))
        .route("/v1/answer/stream", post(answer_stream))
}

async fn answer(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<AnswerApiRequest>,
) -> Result<Response, AppError> {
    // 1. Validate query + prompt (cheap rejections) before any work.
    let query = req.query.trim();
    if !valid_query(query) {
        return Ok(err_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "query must be non-empty and at most 1024 characters",
        ));
    }
    if !valid_prompt(req.prompt.as_deref()) {
        return Ok(err_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "prompt must be at most 4000 characters",
        ));
    }
    let limit = clamp_limit(req.limit);

    // 2. Feature gate: no [llm] → no AnswerService → 501 + `no-store`, mirroring
    //    search.rs::summary_pending_response so a proxy doesn't pin the disabled
    //    state across a restart-with-[llm].
    let Some(svc) = state.answer_service.clone() else {
        return Ok(feature_disabled_response());
    };

    // The metrics timer starts here — after the cheap rejections — so the
    // histogram only counts real attempts and throttles.
    let mut timer = AnswerReqTimer::start(&auth.workspace_id);

    // 3. Per-workspace concurrency gate. Non-blocking acquire: a full gate 429s
    //    immediately rather than queueing behind the deadline. The permit is
    //    held (bound to `_permit`) for the whole handler, including the loop.
    let gate = gate_for(&auth.workspace_id, state.answer_concurrency);
    let _permit = match gate.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            timer.set_outcome("throttled");
            return Ok(err_response(
                StatusCode::TOO_MANY_REQUESTS,
                "THROTTLED",
                "too many concurrent answer requests for this workspace",
            ));
        }
    };

    // 4. Route-level backstop deadline around the whole agentic loop.
    let query_log: String = query.chars().take(64).collect();
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        ANSWER_DEADLINE,
        svc.answer(
            &auth.workspace_id,
            query,
            req.path_prefix.as_deref(),
            limit,
            req.prompt.as_deref(),
        ),
    )
    .await;

    // 5. Map result → wire. `grounded`/`rounds` never leave the process; they
    //    only pick the metrics labels.
    match outcome {
        Err(_elapsed) => {
            timer.set_outcome("timeout");
            Ok(err_response(
                StatusCode::GATEWAY_TIMEOUT,
                "ANSWER_TIMEOUT",
                "answer generation exceeded the deadline",
            ))
        }
        Ok(Ok(r)) => {
            timer.set_outcome(answer_outcome_label(r.grounded, &r.answer));
            record_answer_stats(&auth.workspace_id, r.hit_count, r.estimated_context_tokens, r.rounds);
            info!(
                query = ?query_log,
                hit_count = r.hit_count,
                grounded = r.grounded,
                rounds = r.rounds,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "answer produced"
            );
            Ok(Json(ApiResponse::ok(AnswerApiResponse {
                answer: r.answer,
                citations: r.citations,
                hit_count: r.hit_count,
                estimated_context_tokens: r.estimated_context_tokens,
            }))
            .into_response())
        }
        Ok(Err(AnswerError::LlmFailed(e))) => {
            timer.set_outcome("llm_error");
            warn!(err = %e, "answer: llm failed");
            Ok(err_response(
                StatusCode::BAD_GATEWAY,
                "LLM_UNAVAILABLE",
                "llm upstream unavailable",
            ))
        }
        Ok(Err(AnswerError::Timeout)) => {
            timer.set_outcome("timeout");
            Ok(err_response(
                StatusCode::GATEWAY_TIMEOUT,
                "ANSWER_TIMEOUT",
                "answer generation exceeded the deadline",
            ))
        }
        Ok(Err(AnswerError::Store(e))) => {
            // Store/search failure → existing VedaError→AppError mapping. The
            // timer drops with its default "err" outcome.
            Err(AppError(e))
        }
    }
}

/// POST /v1/answer/stream — SSE variant of `/v1/answer` (same request body).
/// Pre-checks (400/401/429/501) and pre-search errors surface as plain HTTP
/// *before* the stream opens; after that the response is
/// `text/event-stream` with five event types:
///   `delta` `{"text":"…"}`  — incremental LLM output (unaligned `[n]`)
///   `reset` `{}`            — discard all deltas accumulated so far (a
///                             talk-then-tool-call round was rolled back)
///   `tool`  `{"name","detail"}` — a tool call is about to run (progress
///                             only; consumers may render a status line or
///                             ignore it)
///   `final` `{ApiResponse<AnswerApiResponse>}` — authoritative full result
///   `error` `{"error_code","error"}` — failure after the 200 was sent
/// Consumers must replace accumulated deltas with the `final` payload
/// (citations align only against the complete text). The concurrency permit
/// and the metrics timer live inside the stream, so they span generation.
async fn answer_stream(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<AnswerApiRequest>,
) -> Result<Response, AppError> {
    let query = req.query.trim().to_string();
    if !valid_query(&query) {
        return Ok(err_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "query must be non-empty and at most 1024 characters",
        ));
    }
    if !valid_prompt(req.prompt.as_deref()) {
        return Ok(err_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "prompt must be at most 4000 characters",
        ));
    }
    let limit = clamp_limit(req.limit);
    let Some(svc) = state.answer_service.clone() else {
        return Ok(feature_disabled_response());
    };
    let mut timer = AnswerReqTimer::start(&auth.workspace_id);
    let gate = gate_for(&auth.workspace_id, state.answer_concurrency);
    let permit = match gate.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            timer.set_outcome("throttled");
            return Ok(err_response(
                StatusCode::TOO_MANY_REQUESTS,
                "THROTTLED",
                "too many concurrent answer requests for this workspace",
            ));
        }
    };

    // The initial pre-search runs before the SSE opens, so a store failure
    // is still a clean HTTP error, not a mid-stream event.
    let rx = match svc
        .answer_stream(
            &auth.workspace_id,
            &query,
            req.path_prefix.as_deref(),
            limit,
            req.prompt.as_deref(),
        )
        .await
    {
        Ok(rx) => rx,
        // Two failures can land here, both from the pre-SSE retrieve: Store
        // (any VedaError, via the From impl) and Timeout (its 15s budget).
        // The LLM is only touched by the spawned loop task, so LlmFailed can
        // reach the client as a stream event, never as a pre-open error.
        Err(AnswerError::Store(e)) => return Err(AppError(e)),
        Err(AnswerError::Timeout) => {
            timer.set_outcome("timeout");
            return Ok(err_response(
                StatusCode::GATEWAY_TIMEOUT,
                "ANSWER_TIMEOUT",
                "answer generation exceeded the deadline",
            ));
        }
        Err(AnswerError::LlmFailed(_)) => unreachable!("no llm call before the stream task spawns"),
    };

    let ws = auth.workspace_id.clone();
    let query_log: String = query.chars().take(64).collect();
    let stream = async_stream::stream! {
        use std::convert::Infallible;
        let _permit = permit; // held until the stream is dropped
        let mut timer = timer;
        let mut rx = rx;
        let deadline = Instant::now() + ANSWER_DEADLINE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timer.set_outcome("timeout");
                yield Ok::<_, Infallible>(error_event("ANSWER_TIMEOUT", "answer generation exceeded the deadline"));
                return;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(AnswerStreamEvent::Delta(d))) => {
                    yield Ok(delta_event(&d));
                }
                Ok(Some(AnswerStreamEvent::Reset)) => {
                    yield Ok(reset_event());
                }
                Ok(Some(AnswerStreamEvent::ToolNote { name, detail })) => {
                    yield Ok(tool_event(&name, &detail));
                }
                Ok(Some(AnswerStreamEvent::Done(r))) => {
                    timer.set_outcome(answer_outcome_label(r.grounded, &r.answer));
                    record_answer_stats(&ws, r.hit_count, r.estimated_context_tokens, r.rounds);
                    info!(
                        query = ?query_log,
                        hit_count = r.hit_count,
                        grounded = r.grounded,
                        rounds = r.rounds,
                        streamed = true,
                        "answer produced"
                    );
                    yield Ok(final_event(&AnswerApiResponse {
                        answer: r.answer,
                        citations: r.citations,
                        hit_count: r.hit_count,
                        estimated_context_tokens: r.estimated_context_tokens,
                    }));
                    return;
                }
                Ok(Some(AnswerStreamEvent::Failed(e))) => {
                    let (code, msg, outcome) = match e {
                        AnswerError::Timeout => ("ANSWER_TIMEOUT", "llm stalled", "timeout"),
                        AnswerError::LlmFailed(m) => {
                            warn!(err = %m, "answer stream: llm failed mid-stream");
                            ("LLM_UNAVAILABLE", "llm upstream unavailable", "llm_error")
                        }
                        AnswerError::Store(_) => ("INTERNAL", "internal error", "err"),
                    };
                    timer.set_outcome(outcome);
                    yield Ok(error_event(code, msg));
                    return;
                }
                Ok(None) => {
                    // Producer ended without Done/Failed — treat as LLM loss.
                    timer.set_outcome("llm_error");
                    yield Ok(error_event("LLM_UNAVAILABLE", "llm upstream unavailable"));
                    return;
                }
                Err(_elapsed) => {
                    timer.set_outcome("timeout");
                    yield Ok(error_event("ANSWER_TIMEOUT", "answer generation exceeded the deadline"));
                    return;
                }
            }
        }
    };
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

// SSE event builders. `json_data` only fails on unserialisable payloads —
// these are plain serde structs, so fall back to a static error frame rather
// than panicking inside a response stream.
fn delta_event(text: &str) -> Event {
    Event::default()
        .event("delta")
        .json_data(serde_json::json!({ "text": text }))
        .unwrap_or_else(|_| sse_fallback())
}

/// Tells the consumer to drop every delta received so far and start over.
/// Older tunnels ignore unknown events — their interim frames may briefly
/// show discarded preamble, but the final frame is authoritative anyway.
fn reset_event() -> Event {
    Event::default().event("reset").data("{}")
}

/// Progress note for a tool call about to run (name + key argument, no
/// results). Older consumers ignore unknown events, so this is additive.
fn tool_event(name: &str, detail: &str) -> Event {
    Event::default()
        .event("tool")
        .json_data(serde_json::json!({ "name": name, "detail": detail }))
        .unwrap_or_else(|_| sse_fallback())
}

fn final_event(payload: &AnswerApiResponse) -> Event {
    Event::default()
        .event("final")
        .json_data(ApiResponse::ok(payload))
        .unwrap_or_else(|_| sse_fallback())
}

fn error_event(code: &str, msg: &str) -> Event {
    Event::default()
        .event("error")
        .json_data(serde_json::json!({ "error_code": code, "error": msg }))
        .unwrap_or_else(|_| sse_fallback())
}

fn sse_fallback() -> Event {
    Event::default()
        .event("error")
        .data(r#"{"error_code":"INTERNAL","error":"serialize failed"}"#)
}

// ── pure helpers (unit-tested) ─────────────────────────

/// A trimmed query is valid when non-empty and within the char cap.
/// pub(crate): the MCP `ask` tool applies the same constraint.
pub(crate) fn valid_query(trimmed: &str) -> bool {
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_QUERY_CHARS
}

/// Absent prompt is fine (server default persona); present must fit the cap.
fn valid_prompt(prompt: Option<&str>) -> bool {
    prompt.is_none_or(|p| p.chars().count() <= MAX_PROMPT_CHARS)
}

/// Default 12, capped at 24.
fn clamp_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

/// Metrics outcome for a produced answer. `empty` used to mean "retrieval
/// found nothing, LLM never called"; in the agentic loop it means the model
/// itself concluded there is nothing to answer from (fixed refusal phrase).
/// pub(crate): shared with the MCP `ask` tool.
pub(crate) fn answer_outcome_label(grounded: bool, answer: &str) -> &'static str {
    if answer.contains(NO_CONTEXT_ANSWER) {
        "empty"
    } else if grounded {
        "ok"
    } else {
        "ungrounded"
    }
}

// ── responses ──────────────────────────────────────────

fn err_response(status: StatusCode, code: &'static str, msg: &'static str) -> Response {
    (status, Json(ApiResponse::<()>::err(code, msg))).into_response()
}

/// [llm] unconfigured → the feature can never produce an answer. 501 +
/// `no-store` so a proxy doesn't cache the disabled state (same reasoning as
/// search.rs::summary_pending_response).
fn feature_disabled_response() -> Response {
    let body = Json(ApiResponse::<()>::err(
        "FEATURE_DISABLED",
        "answer generation is disabled (server has no [llm] configured)",
    ));
    let mut resp = (StatusCode::NOT_IMPLEMENTED, body).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// ── metrics ────────────────────────────────────────────

/// Per-answer distribution metrics, recorded only on a produced answer.
/// `veda_answer_rounds` watches for a runaway loop (every answer maxing its
/// round cap = prompt or model regression).
/// pub(crate): shared with the MCP `ask` tool so both surfaces feed the
/// same histograms.
pub(crate) fn record_answer_stats(workspace_id: &str, hits: usize, est_tokens: usize, rounds: usize) {
    ::metrics::histogram!("veda_answer_hits", "workspace_id" => workspace_id.to_string())
        .record(hits as f64);
    ::metrics::histogram!(
        "veda_answer_estimated_context_tokens",
        "workspace_id" => workspace_id.to_string()
    )
    .record(est_tokens as f64);
    ::metrics::histogram!("veda_answer_rounds", "workspace_id" => workspace_id.to_string())
        .record(rounds as f64);
}

/// RAII timer for `veda_answer_request_seconds{workspace_id,outcome}`. Defaults
/// to `outcome=err` so an unforeseen early return records as an error; the
/// handler sets ok|empty|ungrounded|llm_error|timeout|throttled explicitly.
/// pub(crate): the MCP `ask` tool records into the same histogram.
pub(crate) struct AnswerReqTimer {
    workspace_id: String,
    started: Instant,
    outcome: &'static str,
}

impl AnswerReqTimer {
    pub(crate) fn start(workspace_id: &str) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            started: Instant::now(),
            outcome: "err",
        }
    }

    pub(crate) fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

impl Drop for AnswerReqTimer {
    fn drop(&mut self) {
        ::metrics::histogram!(
            "veda_answer_request_seconds",
            "workspace_id" => self.workspace_id.clone(),
            "outcome" => self.outcome,
        )
        .record(self.started.elapsed().as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_empty_or_whitespace_is_invalid() {
        assert!(!valid_query("".trim()));
        assert!(!valid_query("   ".trim()));
    }

    #[test]
    fn query_at_cap_is_valid() {
        let q = "a".repeat(MAX_QUERY_CHARS);
        assert!(valid_query(q.trim()));
    }

    #[test]
    fn query_over_cap_is_invalid() {
        let q = "a".repeat(MAX_QUERY_CHARS + 1);
        assert!(!valid_query(q.trim()));
    }

    #[test]
    fn query_cap_counts_chars_not_bytes() {
        // 1024 CJK chars = 3072 bytes, but exactly the cap in chars.
        let ok = "中".repeat(MAX_QUERY_CHARS);
        assert!(valid_query(ok.trim()));
        let over = "中".repeat(MAX_QUERY_CHARS + 1);
        assert!(!valid_query(over.trim()));
    }

    #[test]
    fn prompt_absent_or_within_cap_is_valid() {
        assert!(valid_prompt(None));
        assert!(valid_prompt(Some("你是 DAL 答疑助手")));
        assert!(valid_prompt(Some(&"中".repeat(MAX_PROMPT_CHARS))));
        assert!(!valid_prompt(Some(&"中".repeat(MAX_PROMPT_CHARS + 1))));
    }

    #[test]
    fn outcome_label_prefers_refusal_over_grounded() {
        assert_eq!(answer_outcome_label(true, "答案 [1]"), "ok");
        assert_eq!(answer_outcome_label(false, "无标注答案"), "ungrounded");
        // The refusal phrase wins regardless of grounded (refusals are
        // grounded with zero citations by the aligner).
        assert_eq!(answer_outcome_label(true, NO_CONTEXT_ANSWER), "empty");
    }

    #[test]
    fn limit_default_is_12() {
        assert_eq!(clamp_limit(None), 12);
    }

    #[test]
    fn limit_capped_at_24() {
        assert_eq!(clamp_limit(Some(100)), 24);
    }

    #[test]
    fn limit_passes_through_when_small() {
        assert_eq!(clamp_limit(Some(5)), 5);
    }
}

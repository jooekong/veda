//! RAG answer endpoint (`POST /v1/answer`): retrieve → tiered assembly → LLM
//! generation with verifiable citations. This is a thin route shell — auth,
//! DTO validation, the per-workspace concurrency gate, the total deadline, and
//! error/metrics mapping. All assembly + generation lives in
//! `veda_core::service::answer::AnswerService`.
//!
//! P0 is fs-kind only: the `AuthWorkspace` extractor enforces `kind == Fs` and
//! returns `WORKSPACE_KIND_MISMATCH` (400) for a db `wk_`. See
//! `docs/plans/veda-answer-plan.md` §3 (contract) / §9 (code landing).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use tokio::sync::Semaphore;
use tracing::{info, warn};
use veda_core::service::answer::{AnswerError, AnswerOutcome, NO_CONTEXT_ANSWER};
use veda_types::api::{AnswerApiRequest, AnswerApiResponse};
use veda_types::ApiResponse;

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

/// Query length ceiling (chars, post-trim). Plan §3.
const MAX_QUERY_CHARS: usize = 1024;
/// Retrieval candidate count: default 12, capped at 24. Plan §3.
const DEFAULT_LIMIT: usize = 12;
const MAX_LIMIT: usize = 24;
/// Total wall-clock budget for one answer (retrieve + assemble + LLM, whose own
/// per-attempt timeout is 20s × 1 retry). The route is mounted OUTSIDE the 30s
/// TimeoutLayer (see `routes/mod.rs`) precisely so this longer deadline applies.
const ANSWER_DEADLINE: Duration = Duration::from_secs(45);

/// Per-workspace answer concurrency gates. Process-local (single pod is enough
/// per the simplification convention). A semaphore is created lazily per
/// workspace with `AppState::answer_concurrency` permits, then reused.
static GATES: LazyLock<Mutex<HashMap<String, Arc<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fetch (or lazily create) the concurrency semaphore for a workspace. The lock
/// is held only for the map lookup — never across an await.
fn gate_for(workspace_id: &str, permits: usize) -> Arc<Semaphore> {
    let mut gates = GATES.lock().unwrap();
    gates
        .entry(workspace_id.to_string())
        // `.max(1)` so a misconfigured concurrency=0 doesn't wedge a workspace
        // into permanent 429s.
        .or_insert_with(|| Arc::new(Semaphore::new(permits.max(1))))
        .clone()
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/answer", post(answer))
}

async fn answer(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<AnswerApiRequest>,
) -> Result<Response, AppError> {
    // 1. Validate the query (trim → non-empty → char cap) before any work.
    let query = req.query.trim();
    if !valid_query(query) {
        return Ok(err_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "query must be non-empty and at most 1024 characters",
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
    // histogram only counts real attempts and throttles (plan §10).
    let mut timer = AnswerReqTimer::start(&auth.workspace_id);

    // 3. Per-workspace concurrency gate. Non-blocking acquire: a full gate 429s
    //    immediately rather than queueing behind the deadline. The permit is
    //    held (bound to `_permit`) for the whole handler, including the LLM call.
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

    // 4. Total deadline around the whole retrieve + assemble + LLM pipeline.
    let query_log: String = query.chars().take(64).collect();
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        ANSWER_DEADLINE,
        svc.answer(&auth.workspace_id, query, req.path_prefix.as_deref(), limit),
    )
    .await;

    // 5. Map result → wire. `grounded` never leaves the process; it only picks
    //    the ok/ungrounded metrics label.
    match outcome {
        Err(_elapsed) => {
            timer.set_outcome("timeout");
            Ok(err_response(
                StatusCode::GATEWAY_TIMEOUT,
                "ANSWER_TIMEOUT",
                "answer generation exceeded the deadline",
            ))
        }
        Ok(Ok(AnswerOutcome::Answered(r))) => {
            timer.set_outcome(if r.grounded { "ok" } else { "ungrounded" });
            record_answer_stats(&auth.workspace_id, r.hit_count, r.estimated_context_tokens);
            info!(
                query = %query_log,
                hit_count = r.hit_count,
                grounded = r.grounded,
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
        Ok(Ok(AnswerOutcome::NoContext)) => {
            timer.set_outcome("empty");
            info!(query = %query_log, hit_count = 0, "answer: no relevant context");
            Ok(Json(ApiResponse::ok(AnswerApiResponse {
                answer: NO_CONTEXT_ANSWER.to_string(),
                citations: Vec::new(),
                hit_count: 0,
                estimated_context_tokens: 0,
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

// ── pure helpers (unit-tested) ─────────────────────────

/// A trimmed query is valid when non-empty and within the char cap.
fn valid_query(trimmed: &str) -> bool {
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_QUERY_CHARS
}

/// Default 12, capped at 24 (plan §3).
fn clamp_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
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
fn record_answer_stats(workspace_id: &str, hits: usize, est_tokens: usize) {
    ::metrics::histogram!("veda_answer_hits", "workspace_id" => workspace_id.to_string())
        .record(hits as f64);
    ::metrics::histogram!(
        "veda_answer_estimated_context_tokens",
        "workspace_id" => workspace_id.to_string()
    )
    .record(est_tokens as f64);
}

/// RAII timer for `veda_answer_request_seconds{workspace_id,outcome}`. Defaults
/// to `outcome=err` so an unforeseen early return records as an error; the
/// handler sets ok|empty|ungrounded|llm_error|timeout|throttled explicitly.
struct AnswerReqTimer {
    workspace_id: String,
    started: Instant,
    outcome: &'static str,
}

impl AnswerReqTimer {
    fn start(workspace_id: &str) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            started: Instant::now(),
            outcome: "err",
        }
    }

    fn set_outcome(&mut self, outcome: &'static str) {
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

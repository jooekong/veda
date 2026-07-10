//! Thin veda data-plane client: `POST /v1/search` with a bot's read-only
//! `wk_`. A standard consumer of the HTTP contract — no in-process coupling
//! to veda-core. The contract is anchored in JSON (see §4.6 / §9), so the
//! request/response types here are a deliberate small mirror, NOT
//! veda-types imports.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// Client-side cap (§9): keep search well inside the 10-minute stream window
// so a hung backend can't wedge the reply.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// `/v1/answer` waits on LLM generation, so a single answer request overrides
// the client-wide 10s default with its own 60s cap (§8). Search keeps 10s.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

pub struct VedaClient {
    http: reqwest::Client,
    base_url: String,
}

/// Only the fields `/v1/search` accepts. veda rejects unknown fields
/// (`deny_unknown_fields`), so this struct must stay minimal.
#[derive(Serialize)]
struct SearchReq<'a> {
    query: &'a str,
    mode: &'a str,
    limit: usize,
}

/// Mirror of the public `/v1/search` envelope: `{success, data, error}`.
#[derive(Deserialize)]
struct SearchResp {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<Vec<Hit>>,
    #[serde(default)]
    error: Option<String>,
}

/// A search hit — we only consume `content` + `path`, but keep score fields
/// for logging / future ranking. Extra fields in the wire payload are
/// ignored (forward-compatible).
#[derive(Debug, Deserialize)]
pub struct Hit {
    pub content: String,
    /// `None` when the backend couldn't resolve a live path for the hit.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub score_type: Option<String>,
}

/// Only the fields `/v1/answer` needs. `path_prefix`/`limit` are optional
/// server-side and not sent this phase.
#[derive(Serialize)]
struct AnswerReq<'a> {
    query: &'a str,
}

/// Mirror of the `/v1/answer` envelope: `{success, data, error}`.
#[derive(Deserialize)]
struct AnswerResp {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<AnswerData>,
    #[serde(default)]
    error: Option<String>,
}

/// The answer payload from `/v1/answer`. The wire also carries `hit_count`,
/// `estimated_context_tokens`, and per-citation `spans`, but the tunnel only
/// needs the body + citation paths — unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub struct AnswerData {
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub citations: Vec<AnswerCitation>,
}

/// One citation. Fields are `#[serde(default)]` so a malformed element
/// degrades (index 0 / no path) instead of failing the whole decode.
#[derive(Debug, Deserialize)]
pub struct AnswerCitation {
    /// Server-assigned 1-based index; matches the `[n]` markers in `answer`.
    #[serde(default)]
    pub index: usize,
    /// `None` when the backend couldn't resolve a live path for the citation.
    #[serde(default)]
    pub path: Option<String>,
}

/// Distinguishes "key is dead" (don't drop the WeCom connection, just flag
/// the bot) from "backend hiccup" (transient), so the handler can react
/// per §9. `Disabled`/`Throttled` are answer-only (501/429).
#[derive(Debug)]
pub enum SearchError {
    Unauthorized,
    /// `/v1/answer` returned 501 — the server has no `[llm]` configured.
    Disabled,
    /// `/v1/answer` returned 429 — per-workspace answer concurrency exceeded.
    Throttled,
    Unavailable(String),
}

impl VedaClient {
    pub fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
        })
    }

    pub async fn search(
        &self,
        veda_key: &str,
        query: &str,
        mode: &str,
        limit: usize,
    ) -> Result<Vec<Hit>, SearchError> {
        let url = format!("{}/v1/search", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(veda_key)
            .json(&SearchReq { query, mode, limit })
            .send()
            .await
            .map_err(|e| SearchError::Unavailable(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SearchError::Unauthorized);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Unavailable(format!(
                "HTTP {}: {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            )));
        }

        let parsed: SearchResp = resp
            .json()
            .await
            .map_err(|e| SearchError::Unavailable(format!("decode: {e}")))?;
        if !parsed.success {
            return Err(SearchError::Unavailable(
                parsed.error.unwrap_or_else(|| "search failed".to_string()),
            ));
        }
        Ok(parsed.data.unwrap_or_default())
    }

    /// `POST /v1/answer` — RAG answer with verifiable citations. Uses a
    /// per-request 60s timeout so LLM generation isn't cut off by the client's
    /// 10s default. Status mapping: 401→Unauthorized, 501→Disabled,
    /// 429→Throttled, everything else (incl. 502/504/network/timeout/decode)
    /// →Unavailable.
    pub async fn answer(&self, veda_key: &str, query: &str) -> Result<AnswerData, SearchError> {
        let url = format!("{}/v1/answer", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(veda_key)
            .json(&AnswerReq { query })
            .timeout(ANSWER_TIMEOUT)
            .send()
            .await
            .map_err(|e| SearchError::Unavailable(e.to_string()))?;

        let status = resp.status();
        match status {
            reqwest::StatusCode::UNAUTHORIZED => return Err(SearchError::Unauthorized),
            reqwest::StatusCode::NOT_IMPLEMENTED => return Err(SearchError::Disabled),
            reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(SearchError::Throttled),
            _ => {}
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Unavailable(format!(
                "HTTP {}: {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            )));
        }

        let parsed: AnswerResp = resp
            .json()
            .await
            .map_err(|e| SearchError::Unavailable(format!("decode: {e}")))?;
        if !parsed.success {
            return Err(SearchError::Unavailable(
                parsed.error.unwrap_or_else(|| "answer failed".to_string()),
            ));
        }
        parsed
            .data
            .ok_or_else(|| SearchError::Unavailable("answer: empty data".to_string()))
    }
}

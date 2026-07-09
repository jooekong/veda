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

/// Distinguishes "key is dead" (don't drop the WeCom connection, just flag
/// the bot) from "backend hiccup" (transient), so the handler can react
/// per §9.
#[derive(Debug)]
pub enum SearchError {
    Unauthorized,
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
}

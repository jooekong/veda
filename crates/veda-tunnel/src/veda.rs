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
// `/v1/answer` runs an agentic tool loop server-side (route deadline 90s),
// so a single answer request overrides the client-wide 10s default with its
// own 120s cap — reqwest's per-request timeout covers the WHOLE SSE body
// read, so it must sit above the server's backstop. Search keeps 10s.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

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
/// server-side and not sent this phase. `prompt` is skipped when absent so
/// the wire shape for promptless bots is unchanged (and old servers with
/// `deny_unknown_fields` only ever see it once they support it — deploy
/// order: server first).
#[derive(Serialize)]
struct AnswerReq<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
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

/// The answer payload from `/v1/answer`. The wire also carries
/// `estimated_context_tokens` and per-citation `spans`, which the tunnel
/// ignores — unknown fields are dropped.
#[derive(Debug, Deserialize)]
pub struct AnswerData {
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub citations: Vec<AnswerCitation>,
    /// Retrieval hit count — 0 means the canned no-context reply; >0 with no
    /// citations means an ungrounded answer. Drives QA-log outcome
    /// classification (qa_log.rs).
    #[serde(default)]
    pub hit_count: usize,
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
    /// `/v1/answer/stream` returned 404/405 — the server predates the
    /// streaming endpoint. The handler falls back to the one-shot path, so
    /// tunnel and server can be deployed in either order.
    StreamUnsupported,
    Unavailable(String),
}

/// One event from `POST /v1/answer/stream`. `Final` is authoritative — the
/// consumer replaces accumulated deltas with it (citations only align on the
/// full text, server-side). `Reset` = discard all deltas accumulated so far
/// (the server rolled back a talk-then-tool-call round).
#[derive(Debug)]
pub enum AnswerStreamItem {
    Delta(String),
    Reset,
    Final(AnswerData),
    /// Server-declared failure after the stream opened (error_code), or a
    /// transport break.
    Error(String),
}

/// Splits an SSE byte stream into complete lines across chunk boundaries.
/// Bytes accumulate raw and are decoded only once a line is complete, so a
/// multi-byte char split across network chunks is reassembled before the lossy
/// decode — a mid-char boundary never turns into U+FFFD. Mirrors
/// veda-pipeline/llm.rs::SseLineBuffer — keep behavioural fixes in sync.
#[derive(Default)]
struct SseLineBuffer {
    buf: Vec<u8>,
}

impl SseLineBuffer {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            out.push(line.trim_end_matches(['\n', '\r']).to_string());
        }
        out
    }
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
    pub async fn answer(
        &self,
        veda_key: &str,
        query: &str,
        prompt: Option<&str>,
    ) -> Result<AnswerData, SearchError> {
        let url = format!("{}/v1/answer", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(veda_key)
            .json(&AnswerReq { query, prompt })
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

    /// `POST /v1/answer/stream` — SSE variant. Pre-stream status mapping
    /// matches [`answer`], plus 404/405 → `StreamUnsupported` (older server,
    /// caller falls back to one-shot). Events arrive on the channel; the
    /// parser task ends after `Final`/`Error`, or emits `Error` on transport
    /// break / EOF-without-final. SSE parsing mirrors veda-pipeline/llm.rs —
    /// keep behavioural fixes in sync.
    pub async fn answer_stream(
        &self,
        veda_key: &str,
        query: &str,
        prompt: Option<&str>,
    ) -> Result<tokio::sync::mpsc::Receiver<AnswerStreamItem>, SearchError> {
        use futures_util::StreamExt;

        let url = format!("{}/v1/answer/stream", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(veda_key)
            .json(&AnswerReq { query, prompt })
            .timeout(ANSWER_TIMEOUT)
            .send()
            .await
            .map_err(|e| SearchError::Unavailable(e.to_string()))?;

        let status = resp.status();
        match status {
            reqwest::StatusCode::UNAUTHORIZED => return Err(SearchError::Unauthorized),
            reqwest::StatusCode::NOT_IMPLEMENTED => return Err(SearchError::Disabled),
            reqwest::StatusCode::TOO_MANY_REQUESTS => return Err(SearchError::Throttled),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED => {
                return Err(SearchError::StreamUnsupported)
            }
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

        let (tx, rx) = tokio::sync::mpsc::channel::<AnswerStreamItem>(32);
        let mut bytes = resp.bytes_stream();
        tokio::spawn(async move {
            let mut buf = SseLineBuffer::default();
            let mut event_name = String::new();
            while let Some(chunk) = bytes.next().await {
                let b = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx
                            .send(AnswerStreamItem::Error(format!("stream broke: {e}")))
                            .await;
                        return;
                    }
                };
                for line in buf.push(&b) {
                    let line = line.as_str();
                    if let Some(ev) = line.strip_prefix("event:") {
                        event_name = ev.trim().to_string();
                        continue;
                    }
                    let Some(data) = line.strip_prefix("data:") else {
                        continue; // blank separators, `:` keep-alives
                    };
                    let data = data.trim();
                    match event_name.as_str() {
                        "delta" => {
                            #[derive(Deserialize)]
                            struct D {
                                #[serde(default)]
                                text: String,
                            }
                            if let Ok(d) = serde_json::from_str::<D>(data) {
                                if !d.text.is_empty()
                                    && tx.send(AnswerStreamItem::Delta(d.text)).await.is_err()
                                {
                                    return;
                                }
                            }
                        }
                        "reset" => {
                            if tx.send(AnswerStreamItem::Reset).await.is_err() {
                                return;
                            }
                        }
                        "final" => {
                            // Same {success,data} envelope as the one-shot path.
                            let item = match serde_json::from_str::<AnswerResp>(data) {
                                Ok(p) if p.success && p.data.is_some() => {
                                    AnswerStreamItem::Final(p.data.unwrap())
                                }
                                _ => AnswerStreamItem::Error("bad final frame".to_string()),
                            };
                            let _ = tx.send(item).await;
                            return;
                        }
                        "error" => {
                            #[derive(Deserialize)]
                            struct E {
                                #[serde(default)]
                                error_code: String,
                            }
                            let code = serde_json::from_str::<E>(data)
                                .map(|e| e.error_code)
                                .unwrap_or_else(|_| "UNKNOWN".to_string());
                            let _ = tx.send(AnswerStreamItem::Error(code)).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // EOF without a final frame — the server went away mid-answer.
            let _ = tx
                .send(AnswerStreamItem::Error("closed without final".to_string()))
                .await;
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_line_buffer_reassembles_utf8_split_across_chunks() {
        // A CJK char (3 bytes) split across two network chunks must survive:
        // the buffer decodes only complete lines, so no byte lands mid-char.
        let line = "data: {\"text\":\"中文答案\"}\n";
        let bytes = line.as_bytes();
        let cut = line.find('中').unwrap() + 1; // 1 byte into the first CJK char
        let mut b = SseLineBuffer::default();
        assert!(b.push(&bytes[..cut]).is_empty());
        let lines = b.push(&bytes[cut..]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "data: {\"text\":\"中文答案\"}");
        assert!(!lines[0].contains('\u{FFFD}'), "no replacement char: {}", lines[0]);
    }
}

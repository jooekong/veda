use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;
use veda_core::store::LlmService;
use veda_types::{Result, VedaError};

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    temperature: f32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResp,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResp {
    content: String,
}

pub struct LlmProvider {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    model: String,
}

impl LlmProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| VedaError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            api_url: api_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        })
    }

    async fn chat_once(&self, prompt: &str, max_tokens: usize) -> std::result::Result<String, LlmError> {
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: Some(max_tokens),
            temperature: 0.0,
        };

        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let response = req.send().await.map_err(|e| LlmError {
            inner: VedaError::Internal(format!("LLM request failed: {e}")),
            retryable: true,
        })?;

        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| LlmError {
            inner: VedaError::Internal(format!("LLM read body failed: {e}")),
            retryable: true,
        })?;

        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(LlmError {
                inner: VedaError::Internal(format!("LLM HTTP {status}: {msg}")),
                retryable,
            });
        }

        let parsed: ChatResponse = serde_json::from_slice(&bytes).map_err(|e| LlmError {
            inner: VedaError::Internal(format!("LLM invalid JSON: {e}")),
            retryable: false,
        })?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .ok_or_else(|| LlmError {
                inner: VedaError::Internal("LLM returned empty choices".to_string()),
                retryable: false,
            })
    }

    async fn chat(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            match self.chat_once(prompt, max_tokens).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !e.retryable || attempt == MAX_RETRIES {
                        return Err(e.inner);
                    }
                    let backoff_ms = BASE_BACKOFF_MS * 2u64.pow(attempt);
                    warn!(attempt, backoff_ms, err = %e.inner, "LLM call failed, retrying");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    last_err = Some(e.inner);
                }
            }
        }
        Err(last_err.unwrap())
    }
}

struct LlmError {
    inner: VedaError,
    retryable: bool,
}

#[async_trait]
impl LlmService for LlmProvider {
    async fn summarize(&self, content: &str, max_tokens: usize) -> Result<String> {
        self.chat(content, max_tokens).await
    }
}

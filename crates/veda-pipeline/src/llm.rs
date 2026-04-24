use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use veda_core::store::LlmService;
use veda_types::{Result, VedaError};

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

    async fn chat(&self, prompt: &str, max_tokens: usize) -> Result<String> {
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

        let response = req
            .send()
            .await
            .map_err(|e| VedaError::Internal(format!("LLM request failed: {e}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| VedaError::Internal(format!("LLM read body failed: {e}")))?;

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(VedaError::Internal(format!("LLM HTTP {status}: {msg}")));
        }

        let parsed: ChatResponse = serde_json::from_slice(&bytes)
            .map_err(|e| VedaError::Internal(format!("LLM invalid JSON: {e}")))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .ok_or_else(|| VedaError::Internal("LLM returned empty choices".into()))
    }
}

#[async_trait]
impl LlmService for LlmProvider {
    async fn summarize(&self, content: &str, max_tokens: usize) -> Result<String> {
        self.chat(content, max_tokens).await
    }

    async fn generate_overview(
        &self,
        content: &str,
        child_abstracts: &[String],
    ) -> Result<String> {
        let mut prompt = String::from("Generate a structured overview.\n\nContent:\n");
        prompt.push_str(content);
        if !child_abstracts.is_empty() {
            prompt.push_str("\n\nChild summaries:\n");
            for (i, a) in child_abstracts.iter().enumerate() {
                prompt.push_str(&format!("{}. {}\n", i + 1, a));
            }
        }
        self.chat(&prompt, 2048).await
    }
}

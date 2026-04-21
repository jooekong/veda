use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use veda_core::store::EmbeddingService;
use veda_types::{Result, VedaError};

#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    model: String,
    input: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
}

/// OpenAI-compatible embedding HTTP client.
#[derive(Debug)]
pub struct EmbeddingProvider {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    model: String,
    request_dimensions: Option<u32>,
    configured_dim: Option<usize>,
    discovered_dim: RwLock<Option<usize>>,
}

impl EmbeddingProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: Option<u32>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| VedaError::EmbeddingFailed(e.to_string()))?;

        let configured_dim = dimension.map(|d| d as usize);

        Ok(Self {
            client,
            api_url: api_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            request_dimensions: dimension,
            configured_dim,
            discovered_dim: RwLock::new(None),
        })
    }

    fn resolve_dimension(&self, embedding_len: usize) -> Result<()> {
        if let Some(expected) = self.configured_dim {
            if embedding_len != expected {
                return Err(VedaError::EmbeddingFailed(format!(
                    "embedding length {embedding_len} does not match configured dimension {expected}"
                )));
            }
        } else if let Ok(mut guard) = self.discovered_dim.write() {
            if let Some(d) = *guard {
                if d != embedding_len {
                    return Err(VedaError::EmbeddingFailed(format!(
                        "inconsistent embedding lengths: expected {d}, got {embedding_len}"
                    )));
                }
            } else {
                *guard = Some(embedding_len);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EmbeddingService for EmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = EmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
            dimensions: self.request_dimensions,
        };

        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let response = req
            .send()
            .await
            .map_err(|e| VedaError::EmbeddingFailed(e.to_string()))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| VedaError::EmbeddingFailed(e.to_string()))?;

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(VedaError::EmbeddingFailed(format!("HTTP {status}: {msg}")));
        }

        let parsed: EmbeddingResponse = serde_json::from_slice(&bytes)
            .map_err(|e| VedaError::EmbeddingFailed(format!("invalid embedding JSON: {e}")))?;

        if parsed.data.len() != texts.len() {
            return Err(VedaError::EmbeddingFailed(format!(
                "expected {} embedding rows, got {}",
                texts.len(),
                parsed.data.len()
            )));
        }

        let mut out = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            self.resolve_dimension(item.embedding.len())?;
            out.push(item.embedding);
        }

        Ok(out)
    }

    fn dimension(&self) -> usize {
        if let Some(d) = self.configured_dim {
            return d;
        }
        self.discovered_dim
            .read()
            .ok()
            .and_then(|g| *g)
            .unwrap_or(0)
    }
}

use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;
use veda_core::store::EmbeddingService;
use veda_types::{Result, VedaError};

const BATCH_SIZE: usize = 100;
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;

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

/// OpenAI-compatible embedding HTTP client with batching and retry.
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

    async fn embed_single_batch(
        &self,
        texts: &[String],
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        let body = EmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
            dimensions: self.request_dimensions,
        };

        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let response = req.send().await.map_err(|e| EmbedError {
            inner: VedaError::EmbeddingFailed(e.to_string()),
            retry_after: None,
        })?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let bytes = response.bytes().await.map_err(|e| EmbedError {
            inner: VedaError::EmbeddingFailed(e.to_string()),
            retry_after: None,
        })?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed("rate limited (429)".into()),
                retry_after,
            });
        }

        if status.is_server_error() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!("HTTP {status}: {msg}")),
                retry_after: None,
            });
        }

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!("HTTP {status}: {msg}")),
                retry_after: None,
            });
        }

        let parsed: EmbeddingResponse = serde_json::from_slice(&bytes).map_err(|e| EmbedError {
            inner: VedaError::EmbeddingFailed(format!("invalid embedding JSON: {e}")),
            retry_after: None,
        })?;

        if parsed.data.len() != texts.len() {
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!(
                    "expected {} embedding rows, got {}",
                    texts.len(),
                    parsed.data.len()
                )),
                retry_after: None,
            });
        }

        let mut out = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            self.resolve_dimension(item.embedding.len())
                .map_err(|e| EmbedError {
                    inner: e,
                    retry_after: None,
                })?;
            out.push(item.embedding);
        }
        Ok(out)
    }

    async fn embed_with_retry(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            match self.embed_single_batch(texts).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_retryable(&e.inner) || attempt == MAX_RETRIES {
                        return Err(e.inner);
                    }
                    let backoff_ms = if let Some(secs) = e.retry_after {
                        secs * 1000
                    } else {
                        BASE_BACKOFF_MS * 2u64.pow(attempt)
                    };
                    warn!(attempt, backoff_ms, err = %e.inner, "embedding failed, retrying");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    last_err = Some(e.inner);
                }
            }
        }
        Err(last_err.unwrap())
    }
}

struct EmbedError {
    inner: VedaError,
    retry_after: Option<u64>,
}

fn is_retryable(e: &VedaError) -> bool {
    match e {
        VedaError::EmbeddingFailed(msg) => {
            msg.contains("429")
                || msg.contains("500")
                || msg.contains("502")
                || msg.contains("503")
                || msg.contains("504")
                || msg.contains("connect")
                || msg.contains("timeout")
                || msg.contains("timed out")
        }
        _ => false,
    }
}

#[async_trait]
impl EmbeddingService for EmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        if texts.len() <= BATCH_SIZE {
            return self.embed_with_retry(texts).await;
        }

        let mut all = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH_SIZE) {
            let batch_result = self.embed_with_retry(batch).await?;
            all.extend(batch_result);
        }
        Ok(all)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_catches_rate_limit() {
        let err = VedaError::EmbeddingFailed("rate limited (429), retry-after=1s".into());
        assert!(is_retryable(&err));
    }

    #[test]
    fn is_retryable_catches_server_errors() {
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "HTTP 500: internal".into()
        )));
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "HTTP 502: bad gateway".into()
        )));
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "HTTP 503: unavailable".into()
        )));
    }

    #[test]
    fn is_retryable_catches_connection_errors() {
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "connect error".into()
        )));
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "request timed out".into()
        )));
        assert!(is_retryable(&VedaError::EmbeddingFailed(
            "timeout reached".into()
        )));
    }

    #[test]
    fn is_retryable_rejects_client_errors() {
        assert!(!is_retryable(&VedaError::EmbeddingFailed(
            "HTTP 400: bad request".into()
        )));
        assert!(!is_retryable(&VedaError::EmbeddingFailed(
            "HTTP 401: unauthorized".into()
        )));
        assert!(!is_retryable(&VedaError::EmbeddingFailed(
            "invalid embedding JSON: ...".into()
        )));
    }

    #[test]
    fn is_retryable_rejects_non_embedding_errors() {
        assert!(!is_retryable(&VedaError::NotFound("file".into())));
        assert!(!is_retryable(&VedaError::InvalidInput("bad".into())));
    }

    #[test]
    fn batch_size_constant_is_reasonable() {
        assert_eq!(BATCH_SIZE, 100);
        assert!(BATCH_SIZE < 2048);
    }
}

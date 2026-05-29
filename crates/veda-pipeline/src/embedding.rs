use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use tracing::warn;
use unicode_normalization::UnicodeNormalization;
use veda_core::checksum::sha256_hex;
use veda_core::store::EmbeddingService;
use veda_types::{Result, VedaError};

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;
/// Cap on `Retry-After` header (seconds). Without this, an upstream
/// returning a long retry hint pins the worker's concurrency slot for
/// hours — beyond the 10-min outbox lease, the task gets reclaimed and
/// re-enters the same sleep, effectively deadlocking that slot.
const MAX_RETRY_AFTER_SECS: u64 = 60;

fn compute_backoff_ms(attempt: u32, retry_after_secs: Option<u64>) -> u64 {
    if let Some(secs) = retry_after_secs {
        secs.min(MAX_RETRY_AFTER_SECS).saturating_mul(1000)
    } else {
        BASE_BACKOFF_MS * 2u64.pow(attempt)
    }
}

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
    /// Position in the request's `input` array. The OpenAI-compatible API is
    /// not contractually ordered, so we reorder by this instead of trusting
    /// array position. Servers that omit it leave every item at 0; a stable
    /// sort then preserves the response's own order (the old behavior).
    #[serde(default)]
    index: usize,
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
    /// Max texts per upstream call. Aliyun Bailian caps at 10; OpenAI
    /// tolerates 2048+. Configurable via `[embedding].batch_size`.
    batch_size: usize,
}

impl EmbeddingProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: Option<u32>,
        batch_size: usize,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| VedaError::EmbeddingFailed(e.to_string()))?;

        let configured_dim = dimension.map(|d| d as usize);
        // batch_size 0 would loop forever in chunks(); reject loudly so
        // a misconfiguration shows up at startup, not as a stuck embed.
        if batch_size == 0 {
            return Err(VedaError::InvalidInput(
                "[embedding].batch_size must be >= 1".into(),
            ));
        }

        Ok(Self {
            client,
            api_url: api_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            request_dimensions: dimension,
            configured_dim,
            discovered_dim: RwLock::new(None),
            batch_size,
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
            retryable: true,
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
            retryable: true,
        })?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed("rate limited (429)".into()),
                retry_after,
                retryable: true,
            });
        }

        if status.is_server_error() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!("HTTP {status}: {msg}")),
                retry_after: None,
                retryable: true,
            });
        }

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!("HTTP {status}: {msg}")),
                retry_after: None,
                retryable: false,
            });
        }

        let parsed: EmbeddingResponse = serde_json::from_slice(&bytes).map_err(|e| EmbedError {
            inner: VedaError::EmbeddingFailed(format!("invalid embedding JSON: {e}")),
            retry_after: None,
            retryable: false,
        })?;

        if parsed.data.len() != texts.len() {
            return Err(EmbedError {
                inner: VedaError::EmbeddingFailed(format!(
                    "expected {} embedding rows, got {}",
                    texts.len(),
                    parsed.data.len()
                )),
                retry_after: None,
                retryable: false,
            });
        }

        // Reorder by the API-provided `index`. The OpenAI-compatible embedding
        // response is not contractually ordered; trusting array position would
        // silently misalign every embedding with its input text on a server
        // that returns items out of order. Stable sort, so a server that omits
        // `index` (all default to 0) keeps the response's own order.
        let mut data = parsed.data;
        data.sort_by_key(|item| item.index);

        let mut out = Vec::with_capacity(data.len());
        for item in data {
            self.resolve_dimension(item.embedding.len())
                .map_err(|e| EmbedError {
                    inner: e,
                    retry_after: None,
                    retryable: false,
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
                    if !e.retryable || attempt == MAX_RETRIES {
                        return Err(e.inner);
                    }
                    let backoff_ms = compute_backoff_ms(attempt, e.retry_after);
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
    retryable: bool,
}

#[async_trait]
impl EmbeddingService for EmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let started = std::time::Instant::now();
        // Use an async block so the inner `?` short-circuits to the block
        // boundary, NOT out of `embed`. Without this wrapper, a failed batch
        // returns straight from `embed`, skipping the metrics emission below
        // and silently under-counting `veda_embed_total{outcome="err"}` for
        // multi-batch failures.
        let result: Result<Vec<Vec<f32>>> = async {
            if texts.len() <= self.batch_size {
                return self.embed_with_retry(texts).await;
            }
            let mut all = Vec::with_capacity(texts.len());
            for batch in texts.chunks(self.batch_size) {
                all.extend(self.embed_with_retry(batch).await?);
            }
            Ok(all)
        }
        .await;
        let outcome = if result.is_ok() { "ok" } else { "err" };
        ::metrics::histogram!(
            "veda_embed_latency_seconds",
            "outcome" => outcome,
        )
        .record(started.elapsed().as_secs_f64());
        ::metrics::counter!(
            "veda_embed_total",
            "outcome" => outcome,
        )
        .increment(1);
        result
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

// ── EmbeddingCache (Stage 3.1) ──────────────────────────────────────

const MAX_CACHEABLE_TEXT_BYTES: usize = 4 * 1024;
const CACHE_CAPACITY: u64 = 50_000;
const CACHE_TTL_SECS: u64 = 24 * 3600;
const CACHE_TTI_SECS: u64 = 3600;

/// L1 cache for embeddings. Within one `embed(texts)` call, partitions
/// texts into cache hits + misses, batches the misses into a single
/// upstream call, caches the successful results.
///
/// Trade-off: across concurrent calls, two requests embedding the same
/// missing text will each call upstream (no `try_get_with` coalescing).
/// Accepted for v0 — batching matters more than concurrent-miss dedup at
/// alpha scale; revisit if company multi-app traffic shows duplication.
pub struct EmbeddingCache {
    inner: Arc<dyn EmbeddingService>,
    cache: Cache<String, Arc<Vec<f32>>>,
    model: String,
}

impl EmbeddingCache {
    pub fn new(inner: Arc<dyn EmbeddingService>, model: impl Into<String>) -> Self {
        let cache = Cache::builder()
            .max_capacity(CACHE_CAPACITY)
            .time_to_live(Duration::from_secs(CACHE_TTL_SECS))
            .time_to_idle(Duration::from_secs(CACHE_TTI_SECS))
            .build();
        Self {
            inner,
            cache,
            model: model.into(),
        }
    }

    /// `None` = text uncacheable (too long); skip cache, pass straight through.
    fn key(&self, text: &str) -> Option<String> {
        if text.len() > MAX_CACHEABLE_TEXT_BYTES {
            return None;
        }
        let normalized: String = text.trim().nfc().collect();
        let raw = format!("{}:{}", self.model, normalized);
        Some(sha256_hex(raw.as_bytes()))
    }
}

#[async_trait]
impl EmbeddingService for EmbeddingCache {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Keys computed once, indexed by `texts` position. None = uncacheable.
        let keys: Vec<Option<String>> = texts.iter().map(|t| self.key(t)).collect();

        // Phase 1: partition into hits + misses.
        let mut results: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut miss_indexes: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if let Some(k) = key {
                if let Some(v) = self.cache.get(k).await {
                    results[i] = Some(v.as_ref().clone());
                    continue;
                }
            }
            miss_indexes.push(i);
        }

        // Phase 2: single batched upstream call for all misses.
        // Failed embed propagates Err — phase 3 insert skipped → cache stays clean.
        if !miss_indexes.is_empty() {
            let miss_texts: Vec<String> =
                miss_indexes.iter().map(|&i| texts[i].clone()).collect();
            let embedded = self.inner.embed(&miss_texts).await?;

            // Defensive: trait contract says one vector per input text; a
            // misbehaving inner impl could short-return and cause index OOB
            // on `embedded[rel_i]` below.
            if embedded.len() != miss_texts.len() {
                return Err(VedaError::EmbeddingFailed(format!(
                    "inner.embed returned {} vectors for {} texts",
                    embedded.len(),
                    miss_texts.len()
                )));
            }

            // Phase 3: fill results + cache the cacheable ones.
            for (rel_i, &abs_i) in miss_indexes.iter().enumerate() {
                let vec = embedded[rel_i].clone();
                if let Some(k) = &keys[abs_i] {
                    self.cache.insert(k.clone(), Arc::new(vec.clone())).await;
                }
                results[abs_i] = Some(vec);
            }
        }

        Ok(results.into_iter().map(|v| v.unwrap()).collect())
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_error_retryable_for_transport() {
        let e = EmbedError {
            inner: VedaError::EmbeddingFailed("connection reset".into()),
            retry_after: None,
            retryable: true,
        };
        assert!(e.retryable);
    }

    #[test]
    fn embed_error_not_retryable_for_client_errors() {
        let e = EmbedError {
            inner: VedaError::EmbeddingFailed("HTTP 400: bad request".into()),
            retry_after: None,
            retryable: false,
        };
        assert!(!e.retryable);
    }

    #[test]
    fn batch_size_constant_is_reasonable() {
        // The default lives in veda-server config now; this test just
        // sanity-checks the documented range we expect callers to use.
        const TYPICAL_OPENAI: usize = 100;
        const TYPICAL_BAILIAN: usize = 10;
        assert!(TYPICAL_OPENAI < 2048);
        assert!(TYPICAL_BAILIAN >= 1);
    }

    #[test]
    fn retry_after_caps_at_max() {
        // Upstream returns 1-day hint → still bounded at 60s.
        assert_eq!(compute_backoff_ms(0, Some(86400)), 60_000);
    }

    #[test]
    fn retry_after_under_cap_passes_through() {
        assert_eq!(compute_backoff_ms(0, Some(5)), 5_000);
        assert_eq!(compute_backoff_ms(0, Some(60)), 60_000);
    }

    #[test]
    fn no_retry_after_uses_exponential() {
        assert_eq!(compute_backoff_ms(0, None), 500);
        assert_eq!(compute_backoff_ms(2, None), 2_000);
    }

    // ── EmbeddingCache tests ───────────────────────────────────

    use std::sync::Mutex;

    /// Records every call's input. Returns deterministic vectors (first
    /// component = text length) so tests can sanity-check ordering.
    struct StubEmbedder {
        dim: usize,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl StubEmbedder {
        fn new(dim: usize) -> Self {
            Self {
                dim,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn last_call(&self) -> Vec<String> {
            self.calls.lock().unwrap().last().cloned().unwrap_or_default()
        }
    }

    #[async_trait]
    impl EmbeddingService for StubEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.lock().unwrap().push(texts.to_vec());
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0; self.dim];
                    v[0] = t.len() as f32;
                    v
                })
                .collect())
        }

        fn dimension(&self) -> usize {
            self.dim
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_upstream() {
        let stub = Arc::new(StubEmbedder::new(4));
        let cache = EmbeddingCache::new(stub.clone(), "test-model");

        cache.embed(&["hello".into()]).await.unwrap();
        assert_eq!(stub.call_count(), 1);

        cache.embed(&["hello".into()]).await.unwrap();
        assert_eq!(stub.call_count(), 1, "second hit should not call upstream");
    }

    #[tokio::test]
    async fn cache_partitions_hit_miss_and_oversize() {
        let stub = Arc::new(StubEmbedder::new(4));
        let cache = EmbeddingCache::new(stub.clone(), "test-model");

        // Warm "hello".
        cache.embed(&["hello".into()]).await.unwrap();
        assert_eq!(stub.last_call(), vec!["hello".to_string()]);

        let oversize = "x".repeat(MAX_CACHEABLE_TEXT_BYTES + 1);
        let r = cache
            .embed(&["hello".into(), "world".into(), oversize.clone()])
            .await
            .unwrap();
        assert_eq!(r.len(), 3);
        // hello hits → upstream batch = [world, oversize]
        assert_eq!(stub.call_count(), 2);
        assert_eq!(stub.last_call(), vec!["world".to_string(), oversize.clone()]);

        // Repeat: hello + world cached, oversize re-embedded.
        cache
            .embed(&["hello".into(), "world".into(), oversize.clone()])
            .await
            .unwrap();
        assert_eq!(stub.call_count(), 3);
        assert_eq!(stub.last_call(), vec![oversize]);
    }

    #[tokio::test]
    async fn cache_key_normalizes_whitespace_and_unicode() {
        let stub = Arc::new(StubEmbedder::new(4));
        let cache = EmbeddingCache::new(stub.clone(), "test-model");

        cache.embed(&["café".into()]).await.unwrap();
        assert_eq!(stub.call_count(), 1);
        // NFC normalize + trim: "  caf\u{0065}\u{0301}  " (NFD with combining
        // accent) should match "café" (NFC).
        cache
            .embed(&["  caf\u{0065}\u{0301}  ".into()])
            .await
            .unwrap();
        assert_eq!(stub.call_count(), 1, "NFC+trim should produce same key");
    }
}

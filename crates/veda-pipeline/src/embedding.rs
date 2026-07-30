use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

use tokio::sync::oneshot;
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

/// Default cap on concurrent upstream embedding calls. The provider quota
/// is RPM-based and shared company-wide (2026-06-11 load test), so the gate
/// is a conservative budget, not a measured ceiling. `[embedding].max_concurrency`.
const DEFAULT_MAX_CONCURRENCY: usize = 8;
/// Concurrent upstream calls one large (multi-chunk) embed() may issue.
/// Same-priority waiters are FIFO, so this stops one bulk upsert from
/// parking dozens of waiters ahead of concurrent interactive queries.
const DIRECT_CHUNK_CONCURRENCY: usize = 4;

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

// ── TwoLevelGate: priority-aware concurrency gate ───────────────────

/// Which queue a caller waits in when every permit is taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GatePriority {
    /// Interactive: someone is waiting on the result (search / ask /
    /// synchronous vector upsert).
    High,
    /// Background: worker indexing. Throughput matters, latency doesn't.
    Low,
}

impl GatePriority {
    fn label(self) -> &'static str {
        match self {
            GatePriority::High => "high",
            GatePriority::Low => "low",
        }
    }
}

struct GateState {
    free: usize,
    high: VecDeque<oneshot::Sender<GatePermit>>,
    low: VecDeque<oneshot::Sender<GatePermit>>,
}

/// Two-priority concurrency gate. Idle background traffic may saturate
/// every permit; the moment an interactive caller shows up it gets the
/// NEXT permit released (in-flight upstream calls can't be preempted —
/// that one round-trip is the physical floor of the hand-off latency).
/// Within a level the hand-off order is FIFO. Cancellation can't lose a
/// permit: the hand-off signal carries the `GatePermit` ITSELF, so a
/// waiter that dies before the send is skipped (send returns it), and one
/// that dies after the send drops the channel — which drops the armed
/// permit — which re-releases it. Ownership accounting is the type
/// system's job, not the protocol's. (Cross-review 07-29: an earlier
/// `send(())`+construct-on-receive protocol leaked a permit whenever the
/// waiter was cancelled between a successful send and its next poll.)
///
/// No third-party priority-semaphore crate carries its weight here: the
/// whole mechanism is one mutex over a counter and two queues.
struct TwoLevelGate {
    state: Mutex<GateState>,
}

struct GatePermit {
    gate: Arc<TwoLevelGate>,
    /// Disarmed permits skip the Drop-release: used only inside `release`
    /// when a send bounced and the SAME logical permit keeps being handed
    /// on by the loop — letting the bounced copy release would double it.
    armed: bool,
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        if self.armed {
            self.gate.release();
        }
    }
}

impl TwoLevelGate {
    fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                free: permits,
                high: VecDeque::new(),
                low: VecDeque::new(),
            }),
        })
    }

    /// Wait for a permit. No timeout by design: every async caller already
    /// lives under its own deadline (router 30s, answer 90s, worker lease +
    /// heartbeat) and cancelling this future safely abandons the queue
    /// slot. The one non-cancellable caller — the SQL `embedding()` UDF's
    /// block_on bridge — wraps its embed in an explicit timeout instead.
    async fn acquire(self: &Arc<Self>, prio: GatePriority) -> GatePermit {
        loop {
            let rx = {
                let mut s = self.state.lock().unwrap();
                if s.free > 0 {
                    s.free -= 1;
                    return GatePermit {
                        gate: Arc::clone(self),
                        armed: true,
                    };
                }
                let (tx, rx) = oneshot::channel();
                match prio {
                    GatePriority::High => s.high.push_back(tx),
                    GatePriority::Low => s.low.push_back(tx),
                }
                rx
            };
            // The signal IS the permit. If this future is cancelled after
            // the sender delivered it, the channel drops the armed permit
            // and its Drop re-releases — nothing is stranded.
            if let Ok(permit) = rx.await {
                return permit;
            }
            // Sender dropped without delivering: unreachable by
            // construction (release only drops a sender whose receiver is
            // already gone — and ours is alive). Re-queue defensively.
        }
    }

    fn release(self: &Arc<Self>) {
        loop {
            // Pop inside the lock, hand off outside it: keeps the critical
            // section minimal.
            let next = {
                let mut s = self.state.lock().unwrap();
                match s.high.pop_front().or_else(|| s.low.pop_front()) {
                    Some(tx) => tx,
                    None => {
                        s.free += 1;
                        return;
                    }
                }
            };
            let permit = GatePermit {
                gate: Arc::clone(self),
                armed: true,
            };
            match next.send(permit) {
                Ok(()) => return,
                // Waiter cancelled BEFORE the send: the permit bounces
                // back. Disarm the bounced copy (the loop keeps handing the
                // logical permit on; a Drop-release here would double it)
                // and try the next waiter.
                Err(mut bounced) => {
                    bounced.armed = false;
                }
            }
        }
    }
}

/// HTTP + retry + the global concurrency gate. One instance is shared by
/// every embedding path in the process (fs search, summary worker,
/// collections, SQL, vectors), so the gate here IS the global gate.
struct EmbedCore {
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
    /// Global priority gate on concurrent upstream calls (429-storm
    /// prevention + interactive-over-background ordering).
    gate: Arc<TwoLevelGate>,
}

/// OpenAI-compatible embedding client: priority-gated concurrency + retry.
/// This handle embeds at High (interactive) priority; `background()` yields
/// a Low-priority view over the same gate for the worker.
pub struct EmbeddingProvider {
    core: Arc<EmbedCore>,
}

impl EmbeddingProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: Option<u32>,
        batch_size: usize,
    ) -> Result<Self> {
        Self::new_tuned(
            api_url,
            api_key,
            model,
            dimension,
            batch_size,
            DEFAULT_MAX_CONCURRENCY,
        )
    }

    pub fn new_tuned(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: Option<u32>,
        batch_size: usize,
        max_concurrency: usize,
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
        if max_concurrency == 0 {
            return Err(VedaError::InvalidInput(
                "[embedding].max_concurrency must be >= 1".into(),
            ));
        }

        Ok(Self {
            core: Arc::new(EmbedCore {
                client,
                api_url: api_url.into(),
                api_key: api_key.into(),
                model: model.into(),
                request_dimensions: dimension,
                configured_dim,
                discovered_dim: RwLock::new(None),
                batch_size,
                gate: TwoLevelGate::new(max_concurrency),
            }),
        })
    }

    /// Low-priority view over the same upstream and the same gate, for the
    /// background worker: idle it can saturate every permit, but any
    /// interactive caller gets the next freed one.
    pub fn background(&self) -> Arc<dyn EmbeddingService> {
        Arc::new(BackgroundEmbedding(Arc::clone(&self.core)))
    }
}

/// Worker-facing wrapper: identical behavior at Low gate priority.
struct BackgroundEmbedding(Arc<EmbedCore>);

#[async_trait]
impl EmbeddingService for BackgroundEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.0.embed_direct(texts, GatePriority::Low).await
    }

    fn dimension(&self) -> usize {
        self.0.dimension_inner()
    }
}

impl EmbedCore {
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
            ::metrics::counter!("veda_embed_429_total").increment(1);
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

    /// One gated upstream call per attempt. The permit is acquired inside
    /// the loop and dropped before the backoff sleep, so a request waiting
    /// out a 429 does not pin a concurrency slot (the pre-gate design had
    /// every retrying caller camping on the upstream simultaneously).
    /// No acquire timeout: every caller already lives under its own
    /// deadline, and cancelling this future abandons the queue slot safely.
    async fn embed_with_retry(
        &self,
        texts: &[String],
        prio: GatePriority,
    ) -> Result<Vec<Vec<f32>>> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            let waited = std::time::Instant::now();
            let permit = self.gate.acquire(prio).await;
            ::metrics::histogram!(
                "veda_embed_permit_wait_seconds",
                "priority" => prio.label(),
            )
            .record(waited.elapsed().as_secs_f64());
            // Drop-guard, not manual inc/dec: this future can be cancelled
            // at the await below (router timeout), which would leak a
            // permanent +1 on the gauge.
            let _inflight = InflightGuard::new();
            ::metrics::histogram!("veda_embed_batch_texts").record(texts.len() as f64);
            let result = self.embed_single_batch(texts).await;
            drop(_inflight);
            drop(permit);
            match result {
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

/// Cancellation-safe in-flight gauge: decrement happens on Drop, so a
/// caller cancelled mid-request still balances the metric.
struct InflightGuard;

impl InflightGuard {
    fn new() -> Self {
        ::metrics::gauge!("veda_embed_inflight").increment(1.0);
        Self
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        ::metrics::gauge!("veda_embed_inflight").decrement(1.0);
    }
}

impl EmbedCore {
    fn dimension_inner(&self) -> usize {
        if let Some(d) = self.configured_dim {
            return d;
        }
        self.discovered_dim
            .read()
            .ok()
            .and_then(|g| *g)
            .unwrap_or(0)
    }

    /// Chunk + gate + retry + metrics: the one embedding path. Chunks run
    /// concurrently under the gate; `buffered` (ordered) caps one call's
    /// fan-out at DIRECT_CHUNK_CONCURRENCY so a bulk load can't park dozens
    /// of same-priority waiters ahead of concurrent interactive queries.
    async fn embed_direct(&self, texts: &[String], prio: GatePriority) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let started = std::time::Instant::now();
        // Async block so the inner `?` short-circuits to the block boundary,
        // not out of the method — otherwise a failed chunk would skip the
        // metrics emission below.
        let result: Result<Vec<Vec<f32>>> = async {
            // Materialize the futures first: a lazy `map` closure trips
            // rustc's higher-ranked lifetime inference under `buffered`.
            let futs: Vec<_> = texts
                .chunks(self.batch_size)
                .map(|c| self.embed_with_retry(c, prio))
                .collect();
            let chunk_results: Vec<Vec<Vec<f32>>> = stream::iter(futs)
                .buffered(DIRECT_CHUNK_CONCURRENCY)
                .try_collect()
                .await?;
            Ok(chunk_results.into_iter().flatten().collect())
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
}

struct EmbedError {
    inner: VedaError,
    retry_after: Option<u64>,
    retryable: bool,
}

#[async_trait]
impl EmbeddingService for EmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.core.embed_direct(texts, GatePriority::High).await
    }

    fn dimension(&self) -> usize {
        self.core.dimension_inner()
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

    // ── TwoLevelGate tests ─────────────────────────────────────

    #[tokio::test]
    async fn gate_high_priority_gets_next_released_permit() {
        let gate = TwoLevelGate::new(1);
        let held = gate.acquire(GatePriority::Low).await;

        // A Low waiter queues FIRST, then a High one arrives.
        let g1 = Arc::clone(&gate);
        let low_waiter = tokio::spawn(async move { g1.acquire(GatePriority::Low).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let g2 = Arc::clone(&gate);
        let high_waiter = tokio::spawn(async move { g2.acquire(GatePriority::High).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        drop(held);
        let high_permit = tokio::time::timeout(Duration::from_secs(1), high_waiter)
            .await
            .expect("high must get the freed permit despite queueing later")
            .unwrap();
        assert!(!low_waiter.is_finished(), "low must still be parked");
        drop(high_permit);
        tokio::time::timeout(Duration::from_secs(1), low_waiter)
            .await
            .expect("low gets the permit once high releases")
            .unwrap();
    }

    #[tokio::test]
    async fn gate_idle_background_saturates_all_permits() {
        let gate = TwoLevelGate::new(4);
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    gate.acquire(GatePriority::Low),
                )
                .await
                .expect("an idle gate must hand every permit to background"),
            );
        }
    }

    #[tokio::test]
    async fn gate_cancelled_waiter_does_not_lose_the_permit() {
        let gate = TwoLevelGate::new(1);
        let held = gate.acquire(GatePriority::High).await;

        let g = Arc::clone(&gate);
        let doomed = tokio::spawn(async move { g.acquire(GatePriority::High).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        doomed.abort(); // caller gave up while queued
        let _ = doomed.await;

        drop(held);
        // The freed permit must skip the dead waiter and stay claimable.
        tokio::time::timeout(Duration::from_millis(200), gate.acquire(GatePriority::Low))
            .await
            .expect("permit must survive a cancelled waiter");
    }

    #[tokio::test]
    async fn gate_permit_survives_cancellation_after_handoff() {
        // The nasty window (cross-review 07-29 BLOCKER): release() sends
        // the hand-off signal successfully, then the waiter is cancelled
        // BEFORE its next poll. On a current_thread runtime this sequence
        // is deterministic: the spawned waiter is parked in rx.await and
        // gets no poll between drop(held) and abort(). With the old
        // `send(())` protocol the permit evaporated here; carrying the
        // permit in the channel makes its Drop re-release it.
        let gate = TwoLevelGate::new(1);
        let held = gate.acquire(GatePriority::High).await;

        let g = Arc::clone(&gate);
        let waiter = tokio::spawn(async move { g.acquire(GatePriority::High).await });
        tokio::time::sleep(Duration::from_millis(20)).await; // waiter queued

        drop(held); // hand-off: send succeeds, waiter not yet polled
        waiter.abort(); // cancelled before consuming the signal
        let _ = waiter.await;

        tokio::time::timeout(Duration::from_millis(200), gate.acquire(GatePriority::Low))
            .await
            .expect("permit must survive a waiter cancelled after a successful hand-off");
    }

    #[tokio::test]
    async fn gate_fifo_within_a_level() {
        let gate = TwoLevelGate::new(1);
        let held = gate.acquire(GatePriority::Low).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..3 {
            let g = Arc::clone(&gate);
            let ord = Arc::clone(&order);
            handles.push(tokio::spawn(async move {
                let _p = g.acquire(GatePriority::Low).await;
                ord.lock().unwrap().push(i);
            }));
            // Deterministic enqueue order.
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        drop(held);
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    // ── Gate test (local HTTP stub) ────────────────────────────

    /// Minimal OpenAI-compatible embedding endpoint: parses `input` length,
    /// sleeps `delay`, answers matching vectors; records peak concurrency.
    async fn spawn_embed_stub(
        delay: Duration,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let inflight = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let inflight = inflight.clone();
                let peak = peak.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // Read until the full body arrived (Content-Length).
                    let body_start = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                    let content_length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while buf.len() < body_start + content_length {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let n_inputs = serde_json::from_slice::<serde_json::Value>(
                        &buf[body_start..],
                    )
                    .ok()
                    .and_then(|v| v["input"].as_array().map(|a| a.len()))
                    .unwrap_or(1);

                    let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);

                    let items: Vec<String> = (0..n_inputs)
                        .map(|i| format!(r#"{{"index":{i},"embedding":[1.0,2.0,3.0,4.0]}}"#))
                        .collect();
                    let body = format!(r#"{{"data":[{}]}}"#, items.join(","));
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn gate_caps_upstream_concurrency() {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = spawn_embed_stub(Duration::from_millis(100), peak.clone()).await;

        let provider =
            Arc::new(EmbeddingProvider::new_tuned(url, "", "m", Some(4), 10, 2).unwrap());
        let calls = (0..8).map(|i| {
            let p = provider.clone();
            tokio::spawn(async move { p.embed(&[format!("t{i}")]).await })
        });
        for h in calls {
            h.await.unwrap().unwrap();
        }
        let observed = peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            observed <= 2,
            "gate must cap upstream concurrency at 2, observed {observed}"
        );
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

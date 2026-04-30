//! Prometheus metrics surface.
//!
//! `install()` mounts a `PrometheusRecorder` as the global `metrics::Recorder`
//! exactly once per process. After that, `metrics::counter!`, `gauge!`,
//! `histogram!` macros anywhere in the binary route into Prometheus storage.
//! `MetricsHandle::render()` returns the Prometheus text exposition for the
//! `/v1/metrics` endpoint.
//!
//! Histogram buckets are tuned to common latency / size shapes seen in this
//! codebase (embed RTT ~50ms-30s, LLM RTT ~500ms-120s, fs op ~1ms-500ms).

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

#[derive(Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl MetricsHandle {
    pub fn render(&self) -> String {
        self.inner.render()
    }
}

/// Build and install the global Prometheus recorder. Call once at startup,
/// before any `metrics::*!` macro fires. Subsequent calls panic
/// (`set_global_recorder` rejects duplicates) — the binary should call this
/// exactly once.
/// Axum middleware that records `veda_http_request_duration_seconds`
/// histogram + `veda_http_requests_total` counter, labeled by route /
/// method / status. Routes are taken from `MatchedPath` so dynamic
/// segments don't blow up label cardinality.
pub async fn track_http(req: Request<Body>, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "<unmatched>".to_string());
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();
    let elapsed = started.elapsed().as_secs_f64();
    ::metrics::histogram!(
        "veda_http_request_duration_seconds",
        "route" => route.clone(),
        "method" => method.clone(),
        "status" => status.clone(),
    )
    .record(elapsed);
    ::metrics::counter!(
        "veda_http_requests_total",
        "route" => route,
        "method" => method,
        "status" => status,
    )
    .increment(1);
    response
}

pub fn install() -> MetricsHandle {
    let handle = PrometheusBuilder::new()
        // Tune histogram buckets for any `*_seconds` metric we record:
        // covers the 5ms – 2min range hit by fs ops, embed, and LLM calls.
        .set_buckets_for_metric(
            Matcher::Suffix("_seconds".to_string()),
            &[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
            ],
        )
        .expect("set buckets")
        .install_recorder()
        .expect("install prometheus recorder");
    MetricsHandle { inner: handle }
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// `install` itself can only be called once per process (it sets the
    /// global recorder). Build a non-installed recorder and exercise the
    /// same builder wiring to confirm bucket tuning + render don't panic.
    #[test]
    fn builder_render_smoke() {
        let handle = PrometheusBuilder::new()
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix("_seconds".to_string()),
                &[0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0],
            )
            .expect("set buckets")
            .build_recorder();
        let h = handle.handle();
        // Render before any metric is emitted: must succeed and be empty-ish.
        let body = h.render();
        // Prometheus exposition format is text — render is allowed to be
        // empty when no metrics have been emitted.
        assert!(body.is_empty() || body.contains('\n'));
    }
}

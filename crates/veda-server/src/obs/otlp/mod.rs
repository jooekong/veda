//! OTLP metrics bridge: additionally push veda's Prometheus metrics to the
//! company Monitor Collector over gRPC. Additive — the `/v1/metrics` Prometheus
//! pull endpoint is untouched.
//!
//! Pipeline: render Prometheus text → parse + convert to OTLP
//! `ExportMetricsServiceRequest` (dual-write `attributes` + deprecated
//! `labels`) → push via gRPC to a collector discovered from the company config
//! service (cached, refreshed on failure).
//!
//! Failure isolation: every step returns `Result`; `run` only warns on error —
//! OTLP problems never affect the main service (no panic, no blocking).
//!
//! See `proto/PROVENANCE.md` for the vendored proto and why we don't use
//! opentelemetry-rust.

pub mod convert;
pub mod discovery;
pub mod proto;
pub mod resource;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{watch, Mutex};

use crate::config::OtlpConfig;

use super::MetricsHandle;
use proto::opentelemetry::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use proto::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use proto::opentelemetry::proto::resource::v1::Resource;

/// gRPC deadline for a single Export call (documented default).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pushes metrics to the company Collector. Resource identity is resolved once
/// at construction; the collector list is cached and re-discovered on failure.
pub struct OtlpExporter {
    /// Built once (reads env.yaml + probes ip/host); reused every tick.
    resource: Resource,
    /// Config-service host for discovery (empty when a direct endpoint is set).
    monitor: String,
    appname: String,
    /// Direct "host:port" override; empty = discover via the config service.
    endpoint: String,
    /// Fixed cumulative-window origin (exporter start), epoch nanos. Reset on
    /// process restart, which the Collector handles as a cumulative reset.
    start_unix_nano: u64,
    /// Cached collector list; refreshed on cache miss, cleared on full failure.
    collectors: Mutex<Vec<String>>,
}

impl OtlpExporter {
    /// Build from config. Identity comes from config overrides, falling back to
    /// env.yaml. Returns `None` (disabled, never panics) when there's neither a
    /// direct endpoint nor enough env.yaml info to discover (plan §3.6 Codex #4).
    pub fn from_config(cfg: &OtlpConfig) -> Option<Self> {
        let env = resource::EnvYaml::load(&cfg.env_yaml_path).unwrap_or_default();
        let appname = first_non_empty(&cfg.appname, &env.appname);
        let env_name = first_non_empty(&cfg.env_name, &env.env_name);
        let monitor = first_non_empty(&cfg.monitor, &env.monitor);

        if cfg.endpoint.is_empty() && (monitor.is_empty() || appname.is_empty()) {
            tracing::warn!(
                env_yaml = %cfg.env_yaml_path,
                "OTLP enabled but no direct endpoint and env.yaml lacks monitor/appname; disabling OTLP"
            );
            return None;
        }
        if appname.is_empty() {
            tracing::warn!("OTLP: appname is empty; the platform may not be able to query veda metrics");
        }

        let resource = resource::build_resource(&appname, &env_name, &env.env_level, &env.zone);
        Some(Self {
            resource,
            monitor,
            appname,
            endpoint: cfg.endpoint.clone(),
            start_unix_nano: now_unix_nano(),
            collectors: Mutex::new(Vec::new()),
        })
    }

    /// Background loop: every `interval_secs`, render → convert → push. Exits on
    /// shutdown. Errors are logged, never propagated (main service unaffected).
    pub async fn run(
        self,
        metrics: MetricsHandle,
        mut shutdown_rx: watch::Receiver<bool>,
        interval_secs: u64,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
            }
            let text = metrics.render();
            match self.export_once(&text).await {
                Ok(collector) => tracing::debug!(collector = %collector, "OTLP metrics exported"),
                Err(e) => tracing::warn!(err = %e, "OTLP metrics export failed (retry next tick)"),
            }
        }
    }

    /// Render → convert → push once. Tries collectors in order until one
    /// accepts; returns the accepting collector address.
    pub async fn export_once(&self, prometheus_text: &str) -> anyhow::Result<String> {
        let times = convert::ConvertTimes {
            start_unix_nano: self.start_unix_nano,
            now_unix_nano: now_unix_nano(),
        };
        let request = convert::prometheus_to_otlp(prometheus_text, self.resource.clone(), &times)?;

        let targets = self.resolve_collectors().await?;
        anyhow::ensure!(!targets.is_empty(), "no metrics collectors available");

        let mut last_err: Option<anyhow::Error> = None;
        for collector in &targets {
            match send_to(collector, request.clone()).await {
                Ok(()) => return Ok(collector.clone()),
                Err(e) => {
                    tracing::warn!(collector = %collector, err = %e, "OTLP export to collector failed, trying next");
                    last_err = Some(e);
                }
            }
        }
        // All collectors failed — drop the cache so the next tick re-discovers.
        self.collectors.lock().await.clear();
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("all collectors failed")))
    }

    /// Direct endpoint wins; otherwise return the cached list, discovering +
    /// caching on a miss.
    async fn resolve_collectors(&self) -> anyhow::Result<Vec<String>> {
        if !self.endpoint.is_empty() {
            return Ok(vec![self.endpoint.clone()]);
        }
        {
            let cached = self.collectors.lock().await;
            if !cached.is_empty() {
                return Ok(cached.clone());
            }
        }
        let fresh = discovery::discover_metrics_collectors(&self.monitor, &self.appname).await?;
        *self.collectors.lock().await = fresh.clone();
        Ok(fresh)
    }
}

/// One unary `MetricsService/Export` over plaintext h2c. The internal Collector
/// has no TLS, so the endpoint uses the `http://` scheme.
async fn send_to(collector: &str, request: ExportMetricsServiceRequest) -> anyhow::Result<()> {
    let endpoint = format!("http://{collector}");
    let channel = tonic::transport::Channel::from_shared(endpoint)?
        // connect_timeout bounds a blackholed collector's TCP/h2 handshake;
        // timeout bounds the Export RPC. Together the send is always bounded so
        // a bad collector fails fast and we fall through to the next one.
        .connect_timeout(EXPORT_TIMEOUT)
        .timeout(EXPORT_TIMEOUT)
        .connect()
        .await?;
    MetricsServiceClient::new(channel).export(request).await?;
    Ok(())
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

fn now_unix_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

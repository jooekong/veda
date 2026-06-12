//! MVP end-to-end: emit a counter into a Prometheus recorder, render it, and
//! push once to a REAL company Collector over OTLP gRPC.
//!
//! Ignored — needs the company config service / collector, so run on an
//! internal box (e.g. .161 dogfood):
//!
//!   cargo test -p veda-server --test otlp_send -- --ignored --nocapture
//!
//! Env knobs:
//!   VEDA_OTLP_ENV_YAML_PATH  env.yaml path (default /etc/ddmc/env.yaml) — supplies
//!                       appname / env_name / env_level / zone / monitor.
//!   VEDA_OTLP_ENDPOINT  optional direct "host:port"; set to skip discovery.
//!
//! Success = `MetricsService/Export` returns OK. Then verify on the platform by
//! `appname` (see plan §5 step 3).

use veda_server::config::OtlpConfig;
use veda_server::obs::otlp::OtlpExporter;

#[tokio::test]
#[ignore = "needs real company collector; run on an internal box"]
async fn export_once_to_real_collector() {
    // Box-only: without the company env.yaml there is no agent to discover,
    // so a laptop run can only fail. Skip instead of failing the suite.
    let env_yaml = std::env::var("VEDA_OTLP_ENV_YAML_PATH")
        .unwrap_or_else(|_| "/etc/ddmc/env.yaml".to_string());
    if !std::path::Path::new(&env_yaml).exists() {
        eprintln!("skipping export_once_to_real_collector: {env_yaml} not present (company box only)");
        return;
    }

    // Record a counter into a local (non-global) Prometheus recorder so we get
    // real exposition text without touching the process-wide recorder.
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        metrics::counter!(
            "veda_http_requests_total",
            "route" => "/v1/otlp_probe",
            "method" => "GET",
            "status" => "200",
        )
        .increment(7);
    });
    let text = handle.render();
    assert!(
        text.contains("veda_http_requests_total"),
        "render must contain the probe counter, got:\n{text}"
    );

    // Drive through the same construction path the server uses (from_config),
    // so the test exercises env.yaml resolution + discovery exactly like prod.
    let cfg = OtlpConfig {
        enabled: true,
        interval_secs: 5,
        endpoint: std::env::var("VEDA_OTLP_ENDPOINT").unwrap_or_default(),
        env_yaml_path: std::env::var("VEDA_OTLP_ENV_YAML_PATH")
            .unwrap_or_else(|_| "/etc/ddmc/env.yaml".to_string()),
        appname: String::new(),
        env_name: String::new(),
        monitor: String::new(),
    };
    eprintln!(
        "exporting via config: env_yaml={} endpoint={}",
        cfg.env_yaml_path,
        if cfg.endpoint.is_empty() {
            "<discover>"
        } else {
            &cfg.endpoint
        }
    );

    let exporter =
        OtlpExporter::from_config(&cfg).expect("OTLP exporter should init from env.yaml/config");
    let collector = exporter
        .export_once(&text)
        .await
        .expect("MetricsService/Export should return OK");
    eprintln!("OTLP export OK → {collector}");
}

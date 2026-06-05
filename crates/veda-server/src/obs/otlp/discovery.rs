//! Collector discovery: the company config service hands back the (HA) list of
//! metrics collectors to push to. We GET it once per refresh and round-robin on
//! send failure (see mod.rs). Internal TLS is self-signed — accept invalid
//! certs, matching the documented `curl -sk`.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AgentConfig {
    #[serde(default)]
    collectors: Collectors,
}

#[derive(Debug, Default, Deserialize)]
struct Collectors {
    /// "host:port" entries, e.g. "10.79.11.108:5318".
    #[serde(default)]
    metrics: Vec<String>,
}

/// GET `https://{monitor}/api/v1/configs/{appname}/version/1/agent` and return
/// the metrics collector `host:port` list. `version` in the path is ignored by
/// the service (it returns the current global list).
pub async fn discover_metrics_collectors(
    monitor: &str,
    appname: &str,
) -> anyhow::Result<Vec<String>> {
    let url = format!("https://{monitor}/api/v1/configs/{appname}/version/1/agent");
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let cfg: AgentConfig = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(cfg.collectors.metrics)
}

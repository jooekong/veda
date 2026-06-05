//! The OTLP `Resource` — the identity labels every metric carries so the
//! company platform can find veda. Per OTHER_LANGUAGE_SDK_INTEGRATION.md the
//! query/filter path keys off `appname`/`env_name`/`ip`/`sdk_version`; writing
//! only the OTel-standard `service.name`/`deployment.environment` makes the
//! data unqueryable. Identity comes from /etc/ddmc/env.yaml + the live host.

use super::proto::opentelemetry::proto::common::v1::{any_value, AnyValue, KeyValue};
use super::proto::opentelemetry::proto::resource::v1::Resource;
use serde::Deserialize;

/// Subset of /etc/ddmc/env.yaml we read: resource identity + `monitor` (the
/// config-service host used for collector discovery). Unknown keys ignored.
#[derive(Debug, Default, Deserialize)]
pub struct EnvYaml {
    #[serde(default)]
    pub appname: String,
    #[serde(default)]
    pub env_name: String,
    #[serde(default)]
    pub env_level: String,
    #[serde(default)]
    pub zone: String,
    /// Config-service host, e.g. `paasconf-hw-sh.ddmc-inc.com`.
    #[serde(default)]
    pub monitor: String,
}

impl EnvYaml {
    /// Read + parse env.yaml. Returns `None` on any failure (missing/unreadable/
    /// malformed) — callers degrade to config overrides, never panic.
    pub fn load(path: &str) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_yaml::from_str(&raw).ok()
    }
}

/// Assemble the OTLP Resource. `appname`/`env_name`/`env_level`/`zone` are
/// resolved upstream (config override falling back to env.yaml); ip/host/
/// sdk_version are read from the running process here.
pub fn build_resource(appname: &str, env_name: &str, env_level: &str, zone: &str) -> Resource {
    let ip = local_ip().unwrap_or_default();
    let host = local_hostname();
    let sdk_version = format!("rust-{}", env!("CARGO_PKG_VERSION"));

    let mut attrs = vec![
        // Required by the company platform query/filter path.
        kv("appname", appname),
        kv("env_name", env_name),
        kv("ip", ip),
        kv("sdk_version", sdk_version),
        // Recommended extras.
        kv("sdk.language", "rust".to_string()),
        kv("host", host),
        kv("service.name", appname.to_string()), // OTel-standard mirror
        kv("deployment.environment", env_name.to_string()),
    ];
    if !env_level.is_empty() {
        attrs.push(kv("env_level", env_level.to_string()));
    }
    if !zone.is_empty() {
        attrs.push(kv("zone", zone.to_string()));
    }
    Resource {
        attributes: attrs,
        dropped_attributes_count: 0,
    }
}

fn kv(key: &str, val: impl Into<String>) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(val.into())),
        }),
    }
}

/// Best-effort primary non-loopback IP: open a UDP socket "toward" a routable
/// address and read the local addr the OS picks for egress. Sends nothing
/// (UDP connect only sets a default peer). Tries a few targets so it works on
/// both internal-only and internet-routed hosts.
fn local_ip() -> Option<String> {
    use std::net::UdpSocket;
    for target in ["10.79.11.1:5318", "1.1.1.1:80", "8.8.8.8:80"] {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(target).is_err() {
            continue;
        }
        if let Ok(addr) = sock.local_addr() {
            let ip = addr.ip();
            if !ip.is_loopback() && !ip.is_unspecified() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

fn local_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

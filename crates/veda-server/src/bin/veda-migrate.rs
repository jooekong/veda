//! Standalone schema migration tool. Splits the migrate-on-startup behavior
//! out of `veda-server` so deploys can run the migration once via a Job /
//! one-off command and start the server with `--skip-migrate`. Multi-replica
//! deploys avoid the start-time CREATE TABLE race this way.
//!
//! Usage:
//!   veda-migrate [config.toml]
//!
//! Reads the same TOML / `VEDA_*` env overrides as `veda-server`. Calls
//! `MysqlStore::migrate()`, which is idempotent (CREATE TABLE IF NOT EXISTS
//! plus ALTER TABLE with errno-1060 catch).

use anyhow::Context;
use tracing::info;
use veda_server::config::ServerConfig;
use veda_store::{MysqlStore, PoolConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default to INFO so the migrate output is useful out of the box;
    // RUST_LOG still wins if the operator wants debug or trace.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/server.toml".into());
    let cfg = ServerConfig::load(&config_path).context("load server config")?;

    info!(database = %redact_url(&cfg.mysql.database_url), "veda-migrate connecting to MySQL");

    // Use a small pool for the migrate tool — we don't need the full server
    // pool budget for one schema pass.
    let store = MysqlStore::with_pool_config(
        &cfg.mysql.database_url,
        PoolConfig {
            max_connections: 4,
            min_connections: 0,
            acquire_timeout_secs: 30,
            idle_timeout_secs: 0,
            max_lifetime_secs: 0,
        },
    )
    .await
    .context("connect to MySQL")?;

    info!("running schema migrations");
    store.migrate().await.context("apply migrations")?;
    info!("veda-migrate done");
    Ok(())
}

/// Strip credentials from a `mysql://user:pass@host/db` URL for log display.
fn redact_url(url: &str) -> String {
    if let Some((scheme_user, rest)) = url.split_once("://") {
        if let Some((_creds, host_part)) = rest.split_once('@') {
            return format!("{scheme_user}://***@{host_part}");
        }
    }
    url.to_string()
}

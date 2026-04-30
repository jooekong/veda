use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub jwt_secret: String,
    pub mysql: MysqlConfig,
    pub milvus: MilvusConfig,
    pub embedding: EmbeddingConfig,
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub reconciler: ReconcilerConfig,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Bearer token gating `/v1/metrics`. When `None`, the endpoint returns
    /// 404 — making metrics opt-in via explicit configuration. Prometheus
    /// scrape jobs supply this in their `bearer_token` (or
    /// `bearer_token_file`) directive.
    #[serde(default)]
    pub metrics_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MysqlConfig {
    pub database_url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default)]
    pub min_connections: u32,
    #[serde(default = "default_acquire_timeout")]
    pub acquire_timeout_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_max_lifetime")]
    pub max_lifetime_secs: u64,
}

fn default_max_connections() -> u32 {
    50
}

fn default_acquire_timeout() -> u64 {
    30
}

fn default_idle_timeout() -> u64 {
    600 // drop idle conns after 10 minutes
}

fn default_max_lifetime() -> u64 {
    1800 // recycle every 30 minutes to stay under typical MySQL wait_timeout
}

#[derive(Debug, Deserialize)]
pub struct MilvusConfig {
    pub url: String,
    pub token: Option<String>,
    pub db: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EmbeddingConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub dimension: u32,
}

#[derive(Debug, Deserialize)]
pub struct WorkerConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            batch_size: default_batch_size(),
            poll_interval_secs: default_poll_secs(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ReconcilerConfig {
    #[serde(default = "default_reconciler_enabled")]
    pub enabled: bool,
    /// Pass interval in seconds. Default: 6 hours. Minimum enforced at 60s.
    #[serde(default = "default_reconciler_interval_secs")]
    pub interval_secs: u64,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            enabled: default_reconciler_enabled(),
            interval_secs: default_reconciler_interval_secs(),
        }
    }
}

fn default_reconciler_enabled() -> bool {
    true
}

fn default_reconciler_interval_secs() -> u64 {
    6 * 3600
}

#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: usize,
}

fn default_max_summary_tokens() -> usize {
    2048
}

fn default_listen() -> String {
    "0.0.0.0:3000".into()
}
fn default_enabled() -> bool {
    true
}
fn default_batch_size() -> usize {
    10
}
fn default_poll_secs() -> u64 {
    2
}

impl ServerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&raw)?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    #[cfg(test)]
    pub fn from_toml(raw: &str) -> anyhow::Result<Self> {
        let mut cfg: Self = toml::from_str(raw)?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("VEDA_LISTEN") {
            self.listen = v;
        }
        if let Ok(v) = std::env::var("VEDA_JWT_SECRET") {
            self.jwt_secret = v;
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_URL") {
            self.mysql.database_url = v;
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_MAX_CONNECTIONS") {
            if let Ok(n) = v.parse() {
                self.mysql.max_connections = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_MIN_CONNECTIONS") {
            if let Ok(n) = v.parse() {
                self.mysql.min_connections = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_ACQUIRE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.mysql.acquire_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.mysql.idle_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_MAX_LIFETIME_SECS") {
            if let Ok(n) = v.parse() {
                self.mysql.max_lifetime_secs = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_MILVUS_URL") {
            self.milvus.url = v;
        }
        if let Ok(v) = std::env::var("VEDA_MILVUS_TOKEN") {
            self.milvus.token = Some(v);
        }
        if let Ok(v) = std::env::var("VEDA_EMBEDDING_API_URL") {
            self.embedding.api_url = v;
        }
        if let Ok(v) = std::env::var("VEDA_EMBEDDING_API_KEY") {
            self.embedding.api_key = v;
        }
        if let Ok(v) = std::env::var("VEDA_EMBEDDING_MODEL") {
            self.embedding.model = v;
        }
        if let Ok(v) = std::env::var("VEDA_EMBEDDING_DIMENSION") {
            if let Ok(n) = v.parse() {
                self.embedding.dimension = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_LLM_API_URL") {
            let llm = self.llm.get_or_insert_with(|| LlmConfig {
                api_url: String::new(),
                api_key: String::new(),
                model: String::new(),
                max_summary_tokens: default_max_summary_tokens(),
            });
            llm.api_url = v;
        }
        if let Ok(v) = std::env::var("VEDA_LLM_API_KEY") {
            if let Some(llm) = &mut self.llm {
                llm.api_key = v;
            }
        }
        if let Ok(v) = std::env::var("VEDA_LLM_MODEL") {
            if let Some(llm) = &mut self.llm {
                llm.model = v;
            }
        }
        if let Ok(v) = std::env::var("VEDA_ALLOWED_ORIGINS") {
            let origins: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !origins.is_empty() {
                self.allowed_origins = origins;
            }
        }
        if let Ok(v) = std::env::var("VEDA_RECONCILER_ENABLED") {
            if let Ok(b) = v.parse() {
                self.reconciler.enabled = b;
            }
        }
        if let Ok(v) = std::env::var("VEDA_RECONCILER_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                self.reconciler.interval_secs = n;
            }
        }
        if let Ok(v) = std::env::var("VEDA_METRICS_TOKEN") {
            // An explicitly empty env var means "disable metrics auth", which
            // we don't allow — empty or unset both leave the endpoint 404.
            if !v.is_empty() {
                self.metrics_token = Some(v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    const MINIMAL_TOML: &str = r#"
jwt_secret = "test-secret-that-is-long-enough-32chars!"

[mysql]
database_url = "mysql://localhost/veda"

[milvus]
url = "http://localhost:19530"

[embedding]
api_url = "http://localhost:11434/api/embed"
api_key = "test-key"
model = "nomic-embed-text"
dimension = 768
"#;

    #[test]
    fn load_minimal_toml() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:3000");
        assert_eq!(cfg.jwt_secret, "test-secret-that-is-long-enough-32chars!");
        assert_eq!(cfg.mysql.database_url, "mysql://localhost/veda");
        assert_eq!(cfg.mysql.max_connections, 50);
        assert!(cfg.llm.is_none());
        assert!(cfg.allowed_origins.is_empty());
    }

    #[test]
    fn env_overrides_toml_values() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_JWT_SECRET", "env-secret-override-32chars-long!");
        std::env::set_var("VEDA_MYSQL_URL", "mysql://prod/veda");
        std::env::set_var("VEDA_LISTEN", "0.0.0.0:8080");
        std::env::set_var("VEDA_EMBEDDING_API_KEY", "sk-env");
        std::env::set_var("VEDA_MYSQL_MAX_CONNECTIONS", "100");
        std::env::set_var("VEDA_ALLOWED_ORIGINS", "https://a.com, https://b.com");

        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();

        std::env::remove_var("VEDA_JWT_SECRET");
        std::env::remove_var("VEDA_MYSQL_URL");
        std::env::remove_var("VEDA_LISTEN");
        std::env::remove_var("VEDA_EMBEDDING_API_KEY");
        std::env::remove_var("VEDA_MYSQL_MAX_CONNECTIONS");
        std::env::remove_var("VEDA_ALLOWED_ORIGINS");

        assert_eq!(cfg.jwt_secret, "env-secret-override-32chars-long!");
        assert_eq!(cfg.mysql.database_url, "mysql://prod/veda");
        assert_eq!(cfg.listen, "0.0.0.0:8080");
        assert_eq!(cfg.embedding.api_key, "sk-env");
        assert_eq!(cfg.mysql.max_connections, 100);
        assert_eq!(cfg.allowed_origins, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn env_creates_llm_config_when_absent() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_LLM_API_URL", "http://llm:8080/v1/chat");

        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();

        std::env::remove_var("VEDA_LLM_API_URL");

        assert!(cfg.llm.is_some());
        assert_eq!(cfg.llm.as_ref().unwrap().api_url, "http://llm:8080/v1/chat");
    }

    #[test]
    fn mysql_pool_defaults_match_documented_values() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        assert_eq!(cfg.mysql.max_connections, 50);
        assert_eq!(cfg.mysql.min_connections, 0);
        assert_eq!(cfg.mysql.acquire_timeout_secs, 30);
        assert_eq!(cfg.mysql.idle_timeout_secs, 600);
        assert_eq!(cfg.mysql.max_lifetime_secs, 1800);
    }

    #[test]
    fn mysql_pool_env_overrides() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_MYSQL_MIN_CONNECTIONS", "5");
        std::env::set_var("VEDA_MYSQL_ACQUIRE_TIMEOUT_SECS", "10");
        std::env::set_var("VEDA_MYSQL_IDLE_TIMEOUT_SECS", "120");
        std::env::set_var("VEDA_MYSQL_MAX_LIFETIME_SECS", "60");

        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();

        std::env::remove_var("VEDA_MYSQL_MIN_CONNECTIONS");
        std::env::remove_var("VEDA_MYSQL_ACQUIRE_TIMEOUT_SECS");
        std::env::remove_var("VEDA_MYSQL_IDLE_TIMEOUT_SECS");
        std::env::remove_var("VEDA_MYSQL_MAX_LIFETIME_SECS");

        assert_eq!(cfg.mysql.min_connections, 5);
        assert_eq!(cfg.mysql.acquire_timeout_secs, 10);
        assert_eq!(cfg.mysql.idle_timeout_secs, 120);
        assert_eq!(cfg.mysql.max_lifetime_secs, 60);
    }

    #[test]
    fn metrics_token_unset_by_default() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        assert!(cfg.metrics_token.is_none(), "default must disable metrics");
    }

    #[test]
    fn metrics_token_from_env() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("VEDA_METRICS_TOKEN", "scrape-token-xyz");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_METRICS_TOKEN");
        assert_eq!(cfg.metrics_token.as_deref(), Some("scrape-token-xyz"));
    }

    #[test]
    fn metrics_token_empty_env_stays_disabled() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("VEDA_METRICS_TOKEN", "");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_METRICS_TOKEN");
        assert!(
            cfg.metrics_token.is_none(),
            "empty env must not silently enable an empty-token bypass"
        );
    }

    #[test]
    fn metrics_token_from_toml() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        // Prepend (not append) — appending would land the field inside the
        // trailing `[embedding]` table of MINIMAL_TOML.
        let toml_with_token = format!("metrics_token = \"toml-token\"\n{MINIMAL_TOML}");
        let cfg = ServerConfig::from_toml(&toml_with_token).unwrap();
        assert_eq!(cfg.metrics_token.as_deref(), Some("toml-token"));
    }

    #[test]
    fn reconciler_defaults_and_env_overrides() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        assert!(cfg.reconciler.enabled);
        assert_eq!(cfg.reconciler.interval_secs, 6 * 3600);

        std::env::set_var("VEDA_RECONCILER_ENABLED", "false");
        std::env::set_var("VEDA_RECONCILER_INTERVAL_SECS", "300");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_RECONCILER_ENABLED");
        std::env::remove_var("VEDA_RECONCILER_INTERVAL_SECS");
        assert!(!cfg.reconciler.enabled);
        assert_eq!(cfg.reconciler.interval_secs, 300);
    }

    #[test]
    fn invalid_numeric_env_is_ignored() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_MYSQL_MAX_CONNECTIONS", "not-a-number");
        std::env::set_var("VEDA_EMBEDDING_DIMENSION", "abc");

        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();

        std::env::remove_var("VEDA_MYSQL_MAX_CONNECTIONS");
        std::env::remove_var("VEDA_EMBEDDING_DIMENSION");

        assert_eq!(cfg.mysql.max_connections, 50);
        assert_eq!(cfg.embedding.dimension, 768);
    }

    #[test]
    fn empty_allowed_origins_env_stays_permissive() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_ALLOWED_ORIGINS", "");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_ALLOWED_ORIGINS");

        assert!(
            cfg.allowed_origins.is_empty(),
            "empty env should not produce vec![\"\"]"
        );
    }

    #[test]
    fn allowed_origins_with_trailing_comma() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

        std::env::set_var("VEDA_ALLOWED_ORIGINS", "https://a.com,,https://b.com,");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_ALLOWED_ORIGINS");

        assert_eq!(cfg.allowed_origins, vec!["https://a.com", "https://b.com"]);
    }
}

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
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct MysqlConfig {
    pub database_url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    50
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
        let _lock = ENV_MUTEX.lock().unwrap();
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
        let _lock = ENV_MUTEX.lock().unwrap();

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
        let _lock = ENV_MUTEX.lock().unwrap();

        std::env::set_var("VEDA_LLM_API_URL", "http://llm:8080/v1/chat");

        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();

        std::env::remove_var("VEDA_LLM_API_URL");

        assert!(cfg.llm.is_some());
        assert_eq!(cfg.llm.as_ref().unwrap().api_url, "http://llm:8080/v1/chat");
    }

    #[test]
    fn invalid_numeric_env_is_ignored() {
        let _lock = ENV_MUTEX.lock().unwrap();

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
        let _lock = ENV_MUTEX.lock().unwrap();

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
        let _lock = ENV_MUTEX.lock().unwrap();

        std::env::set_var("VEDA_ALLOWED_ORIGINS", "https://a.com,,https://b.com,");
        let cfg = ServerConfig::from_toml(MINIMAL_TOML).unwrap();
        std::env::remove_var("VEDA_ALLOWED_ORIGINS");

        assert_eq!(cfg.allowed_origins, vec!["https://a.com", "https://b.com"]);
    }
}

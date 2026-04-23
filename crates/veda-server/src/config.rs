use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub jwt_secret: String,
    pub mysql: MysqlConfig,
    pub milvus: MilvusConfig,
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
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
        Ok(toml::from_str(&raw)?)
    }
}

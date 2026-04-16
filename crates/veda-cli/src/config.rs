use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_server_url")]
    pub server_url: String,
    pub api_key: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_key: Option<String>,
}

fn default_server_url() -> String {
    "http://localhost:9009".into()
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            api_key: None,
            workspace_id: None,
            workspace_key: None,
        }
    }
}

impl CliConfig {
    fn config_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("cannot find config directory")?
            .join("veda");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&raw)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(&path, raw)?;
        Ok(())
    }

    pub fn api_key(&self) -> Result<&str> {
        match &self.api_key {
            Some(k) => Ok(k),
            None => bail!("no API key configured. Run `veda account create` or `veda account login` first."),
        }
    }

    pub fn ws_key(&self) -> Result<&str> {
        match &self.workspace_key {
            Some(k) => Ok(k),
            None => bail!("no workspace selected. Run `veda workspace use <id>` first."),
        }
    }
}

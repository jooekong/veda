use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_server_url")]
    pub server_url: String,
    pub api_key: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_key: Option<String>,
}

fn default_server_url() -> String {
    "http://localhost:3000".into()
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
    /// Default config file path: `${XDG_CONFIG_HOME:-~/.config}/veda/config.toml`
    /// (or the platform equivalent). Creates the parent directory if missing.
    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("cannot find config directory")?
            .join("veda");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::default_path()?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path()?)
    }

    /// Read config from a specific path. Returns default if the file is
    /// absent. Used by tests to inject a tempdir.
    pub fn load_from(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&raw)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Write config to a specific path with mode 0600 on Unix.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn api_key(&self) -> Result<&str> {
        match &self.api_key {
            Some(k) => Ok(k),
            None => bail!(
                "no API key configured. Run `veda init` (or `veda account login`) first."
            ),
        }
    }

    pub fn ws_key(&self) -> Result<&str> {
        match &self.workspace_key {
            Some(k) => Ok(k),
            None => bail!("no workspace selected. Run `veda init` (or `veda workspace use <id>`) first."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_from_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let cfg = CliConfig::load_from(&path).unwrap();
        assert_eq!(cfg.server_url, default_server_url());
        assert!(cfg.api_key.is_none());
        assert!(cfg.workspace_id.is_none());
        assert!(cfg.workspace_key.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let saved = CliConfig {
            server_url: "http://example.com".into(),
            api_key: Some("ak-123".into()),
            workspace_id: Some("ws-uuid".into()),
            workspace_key: Some("wk-789".into()),
        };
        saved.save_to(&path).unwrap();
        let loaded = CliConfig::load_from(&path).unwrap();
        assert_eq!(loaded.server_url, "http://example.com");
        assert_eq!(loaded.api_key.as_deref(), Some("ak-123"));
        assert_eq!(loaded.workspace_id.as_deref(), Some("ws-uuid"));
        assert_eq!(loaded.workspace_key.as_deref(), Some("wk-789"));
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        CliConfig::default().save_to(&path).unwrap();
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        CliConfig::default().save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn api_key_helper_errors_when_missing() {
        let cfg = CliConfig::default();
        let err = cfg.api_key().unwrap_err().to_string();
        assert!(err.contains("veda init"), "msg: {err}");
    }

    #[test]
    fn ws_key_helper_errors_when_missing() {
        let cfg = CliConfig::default();
        let err = cfg.ws_key().unwrap_err().to_string();
        assert!(err.contains("veda init"), "msg: {err}");
    }
}

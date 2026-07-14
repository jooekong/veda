//! Tunnel configuration: `config/tunnel.toml`.
//!
//! One process serves N WeCom bots; each bot binds one read-only `wk_`
//! (one workspace). Config is the single source of truth — there is no
//! dynamic bot store. See docs/plans/veda-tunnel-plan.md §6.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TunnelConfig {
    /// veda data-plane base URL (internal), e.g. `http://10.79.55.85:3000`.
    pub veda_base_url: String,
    /// MySQL for the bot store (`veda_tunnel_bots`), same instance as veda.
    pub mysql: MysqlConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    /// Bots here are a one-time **seed**: on first start with an empty
    /// `veda_tunnel_bots` table they're imported into MySQL. After that the
    /// DB is the source of truth (managed via the admin CRUD API) and this
    /// list is ignored. May be omitted entirely.
    #[serde(default)]
    pub wecom: WecomConfig,
    /// `[answer]` feature switch — see [`AnswerConfig`].
    #[serde(default)]
    pub answer: AnswerConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MysqlConfig {
    /// e.g. `mysql://user:pass@host:3306/veda`.
    pub database_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfig {
    #[serde(default = "default_admin_listen")]
    pub listen: String,
    /// Admin bearer token. When `None`/empty every `/admin/*` route 404s
    /// (fail-closed, matching veda-server's admin surface).
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            listen: default_admin_listen(),
            token: None,
        }
    }
}

fn default_admin_listen() -> String {
    "127.0.0.1:9100".to_string()
}

/// `[answer]` section — a process-wide switch (not per-bot). When `enabled`,
/// text questions are routed through veda's `/v1/answer` RAG endpoint;
/// otherwise the tunnel falls back to raw `/v1/search` + snippet rendering.
///
/// NOTE: read once at process start. The admin Reload only re-reads the MySQL
/// bot list, not this config file, so changing `enabled` requires a process
/// restart to take effect.
#[derive(Debug, Clone, Deserialize)]
pub struct AnswerConfig {
    /// Route questions through `/v1/answer` when true. A missing `[answer]`
    /// section and a missing `enabled` key both default to true.
    #[serde(default = "default_answer_enabled")]
    pub enabled: bool,
}

impl Default for AnswerConfig {
    fn default() -> Self {
        Self {
            enabled: default_answer_enabled(),
        }
    }
}

fn default_answer_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WecomConfig {
    #[serde(default)]
    pub bot: Vec<BotConfig>,
}

// PartialEq: the store-poll reconciler diffs desired (MySQL) vs running
// (in-memory) configs to decide which bots to respawn.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BotConfig {
    /// Human-readable name — admin display + logs.
    pub name: String,
    pub bot_id: String,
    pub secret: String,
    /// Read-only workspace key (`wk_...`) this bot searches with.
    pub veda_key: String,
    /// Human-readable workspace label — display-only metadata (§10.4);
    /// a `wk_` can't be reversed to its workspace name.
    pub workspace: String,
    /// Optional business-line tag — display-only (§10.4). Does NOT trigger
    /// platform project search / gateway authz.
    #[serde(default)]
    pub project: Option<String>,
    /// Search mode passed through to `/v1/search`: `hybrid|semantic|fulltext`.
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Custom answer persona, passed through to `/v1/answer` as `prompt`.
    /// None/empty → the server's default persona.
    #[serde(default)]
    pub prompt: Option<String>,
}

fn default_mode() -> String {
    "hybrid".to_string()
}

fn default_limit() -> usize {
    8
}

impl TunnelConfig {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading tunnel config {}", path.display()))?;
        let cfg: TunnelConfig = toml::from_str(&text)
            .with_context(|| format!("parsing tunnel config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.veda_base_url.trim().is_empty() {
            bail!("veda_base_url is required");
        }
        if self.mysql.database_url.trim().is_empty() {
            bail!("mysql.database_url is required");
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for b in &self.wecom.bot {
            b.validate()?;
            if b.secret.trim().is_empty() {
                bail!("seed bot '{}' has empty secret", b.name);
            }
            if b.veda_key.trim().is_empty() {
                bail!("seed bot '{}' has empty veda_key", b.name);
            }
            // Two bots sharing a bot_id fight over the same single WeCom
            // connection (new-kicks-old) → endless reconnect storm.
            if !ids.insert(b.bot_id.as_str()) {
                bail!("duplicate bot_id '{}' — each bot needs its own connection", b.bot_id);
            }
            if !names.insert(b.name.as_str()) {
                bail!("duplicate bot name '{}' — must be unique for admin", b.name);
            }
        }
        Ok(())
    }
}

impl BotConfig {
    /// Per-bot field validation, shared by config seed and the admin CRUD
    /// API. Secret is checked by callers (required on add/seed, empty=keep on
    /// update); uniqueness is enforced by MySQL (store) or
    /// `TunnelConfig::validate` (seed list).
    pub fn validate(&self) -> anyhow::Result<()> {
        // secret / veda_key are checked by callers (required on add/seed,
        // empty=keep on update), so they're deliberately not here.
        for (field, val) in [
            ("name", &self.name),
            ("bot_id", &self.bot_id),
            ("workspace", &self.workspace),
        ] {
            if val.trim().is_empty() {
                bail!("bot '{}' has empty {}", self.name, field);
            }
        }
        if !matches!(self.mode.as_str(), "hybrid" | "semantic" | "fulltext") {
            bail!(
                "bot '{}' has invalid mode '{}' (want hybrid|semantic|fulltext)",
                self.name,
                self.mode
            );
        }
        // Mirror of the server-side /v1/answer prompt cap — reject early so a
        // too-long persona fails at config time, not per message.
        if let Some(p) = &self.prompt {
            if p.chars().count() > 4000 {
                bail!("bot '{}' prompt exceeds 4000 characters", self.name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> anyhow::Result<TunnelConfig> {
        // Tests omit [mysql] for brevity; inject a dummy so serde is satisfied
        // and validation targets the field under test.
        let full = format!("mysql.database_url = \"mysql://x/db\"\n{s}");
        let cfg: TunnelConfig = toml::from_str(&full)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn defaults_apply() {
        let cfg = parse(
            r#"
            veda_base_url = "http://x:3000"
            [[wecom.bot]]
            name = "a"
            bot_id = "b1"
            secret = "s"
            veda_key = "wk_1"
            workspace = "ws"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.admin.listen, "127.0.0.1:9100");
        assert_eq!(cfg.wecom.bot[0].mode, "hybrid");
        assert_eq!(cfg.wecom.bot[0].limit, 8);
        assert!(cfg.wecom.bot[0].project.is_none());
    }

    #[test]
    fn rejects_duplicate_bot_id() {
        let err = parse(
            r#"
            veda_base_url = "http://x:3000"
            [[wecom.bot]]
            name = "a"
            bot_id = "dup"
            secret = "s"
            veda_key = "wk_1"
            workspace = "ws"
            [[wecom.bot]]
            name = "b"
            bot_id = "dup"
            secret = "s"
            veda_key = "wk_2"
            workspace = "ws2"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate bot_id"));
    }

    #[test]
    fn rejects_empty_required_field() {
        let err = parse(
            r#"
            veda_base_url = "http://x:3000"
            [[wecom.bot]]
            name = "a"
            bot_id = "b1"
            secret = ""
            veda_key = "wk_1"
            workspace = "ws"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty secret"));
    }

    #[test]
    fn rejects_missing_veda_base_url() {
        let err = parse(r#"veda_base_url = "  ""#).unwrap_err();
        assert!(err.to_string().contains("veda_base_url is required"));
    }

    #[test]
    fn rejects_invalid_mode() {
        let err = parse(
            r#"
            veda_base_url = "http://x:3000"
            [[wecom.bot]]
            name = "a"
            bot_id = "b1"
            secret = "s"
            veda_key = "wk_1"
            workspace = "ws"
            mode = "fuzzy"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid mode"));
    }

    #[test]
    fn answer_enabled_defaults_true() {
        // No [answer] section at all → the whole section defaults on.
        let cfg = parse(r#"veda_base_url = "http://x:3000""#).unwrap();
        assert!(cfg.answer.enabled);
    }

    #[test]
    fn answer_can_be_disabled() {
        let cfg = parse(
            r#"
            veda_base_url = "http://x:3000"
            [answer]
            enabled = false
            "#,
        )
        .unwrap();
        assert!(!cfg.answer.enabled);
    }
}

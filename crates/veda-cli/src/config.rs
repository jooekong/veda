use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One workspace profile entry: a workspace id (server-side UUID) paired
/// with the wk_ key minted against it. `id` is Option because the
/// "paste an existing wk_" flow has no way to learn which workspace
/// the key belongs to — there is no server lookup endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub key: String,
}

/// In-memory CLI configuration. The on-disk shape is `RawConfig`; the
/// two diverge to keep legacy migration logic out of the hot path —
/// load_from() normalises old top-level `workspace_id` / `workspace_key`
/// into a `[workspaces.default]` entry with `active_workspace = default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    pub server_url: String,
    pub api_key: Option<String>,
    pub active_workspace: Option<String>,
    pub workspaces: BTreeMap<String, WorkspaceEntry>,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            api_key: None,
            active_workspace: None,
            workspaces: BTreeMap::new(),
        }
    }
}

/// On-disk shape: tolerates legacy top-level `workspace_id` /
/// `workspace_key` so existing configs keep loading. New writes never
/// emit those fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RawConfig {
    #[serde(default = "default_server_url")]
    server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_workspace: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workspaces: BTreeMap<String, WorkspaceEntry>,
    /// Legacy: pre-multi-workspace had a single `workspace_id` at the
    /// top level. Read only; never written back (skip_serializing).
    #[serde(default, skip_serializing)]
    workspace_id: Option<String>,
    /// Legacy: pre-multi-workspace had a single `workspace_key` at
    /// the top level. Read only; never written back (skip_serializing).
    #[serde(default, skip_serializing)]
    workspace_key: Option<String>,
}

fn default_server_url() -> String {
    "http://localhost:3000".into()
}

/// Alias used by migration and onboarding for the implicit default
/// profile. Kept as a constant so a typo would cause a compile error.
pub const DEFAULT_WORKSPACE_ALIAS: &str = "default";

impl CliConfig {
    /// Default config file path: `${XDG_CONFIG_HOME:-~/.config}/veda/config.toml`
    /// on every platform. Deliberately ignore macOS's
    /// `~/Library/Application Support` convention so docs/SOPs that hard-code
    /// `~/.config/veda` work the same on Mac and Linux. Creates the parent
    /// directory if missing.
    pub fn default_path() -> Result<PathBuf> {
        let dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
            .context("cannot determine home directory")?
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
    /// absent. Migrates legacy top-level workspace fields on the fly.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw: RawConfig = if path.exists() {
            let s = std::fs::read_to_string(path)?;
            toml::from_str(&s).context("parse config.toml")?
        } else {
            RawConfig {
                server_url: default_server_url(),
                ..RawConfig::default()
            }
        };
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: RawConfig) -> Self {
        let RawConfig {
            server_url,
            api_key,
            active_workspace,
            mut workspaces,
            workspace_id,
            workspace_key,
        } = raw;

        // Legacy migration: when the file pre-dates multi-workspace
        // it has a single `workspace_id` + `workspace_key` at the
        // top level. Promote them into a `[workspaces.default]`
        // entry, but only when there is no new-shape map at all —
        // otherwise a hand-edited config that mixed both shapes
        // would silently revive a stale legacy workspace (e.g. a
        // file with `[workspaces.work]` and an old top-level
        // `workspace_key` would otherwise inject a phantom
        // `default` profile). When both shapes exist we treat the
        // new shape as canonical and ignore the legacy fields.
        let mut active = active_workspace;
        if workspaces.is_empty() && workspace_key.is_some() {
            workspaces.insert(
                DEFAULT_WORKSPACE_ALIAS.into(),
                WorkspaceEntry {
                    id: workspace_id,
                    key: workspace_key.unwrap_or_default(),
                },
            );
            if active.is_none() {
                active = Some(DEFAULT_WORKSPACE_ALIAS.into());
            }
        }

        Self {
            server_url,
            api_key,
            active_workspace: active,
            workspaces,
        }
    }

    fn to_raw(&self) -> RawConfig {
        RawConfig {
            server_url: self.server_url.clone(),
            api_key: self.api_key.clone(),
            active_workspace: self.active_workspace.clone(),
            workspaces: self.workspaces.clone(),
            workspace_id: None,
            workspace_key: None,
        }
    }

    /// Write config to a specific path with mode 0600 on Unix. Always
    /// emits the new (multi-workspace) shape — legacy fields are
    /// stripped, so reading + saving in place is the migration.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = self.to_raw();
        let s = toml::to_string_pretty(&raw)?;
        std::fs::write(path, s)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn api_key(&self) -> Result<&str> {
        match &self.api_key {
            Some(k) if !k.is_empty() => Ok(k),
            _ => bail!("no API key configured. Run `veda init` (or `veda login --api-key vk_…`) first."),
        }
    }

    /// Borrow the workspace entry for the given alias. Errors if the
    /// alias isn't present in `[workspaces.<alias>]`.
    pub fn workspace_for(&self, alias: &str) -> Result<&WorkspaceEntry> {
        self.workspaces.get(alias).ok_or_else(|| {
            anyhow!(
                "no such workspace profile '{alias}'. \
                 Run `veda workspace list` to see configured profiles."
            )
        })
    }

    /// Alias of the active profile (or `None` if no workspace is
    /// selected yet).
    pub fn active_alias(&self) -> Option<&str> {
        self.active_workspace.as_deref()
    }

    /// Active profile's wk_ key. Used by every data-plane command.
    /// Errors if no profile is selected, the selected alias is
    /// missing from `[workspaces.…]`, or the entry's key is empty
    /// (which happens when `veda init` reached the workspace-create
    /// step but its key-mint failed — better to surface that locally
    /// than send a blank Bearer to the server and get 401).
    pub fn active_wk(&self) -> Result<&str> {
        let alias = self
            .active_alias()
            .ok_or_else(|| anyhow!("no workspace selected. Run `veda init` first."))?;
        let entry = self.workspace_for(alias)?;
        if entry.key.is_empty() {
            // `veda workspace add <alias>` knows about empty-key
            // placeholders (the repair path) and finishes the
            // onboarding without re-creating the server-side
            // workspace. No extra flag required — just the alias.
            // When there's no stored id at all, only `veda init`
            // can recover (paste-wk_ flow shouldn't ever land here
            // because apply_workspace_key always writes a non-empty
            // key, but we still degrade gracefully).
            let retry_hint = if entry.id.is_some() {
                format!("Retry with `veda workspace add {alias}` to finish setup.")
            } else {
                format!("Rerun `veda init` to finish onboarding alias '{alias}'.")
            };
            bail!(
                "workspace '{alias}' has no key — onboarding likely failed mid-flight. \
                 {retry_hint}"
            );
        }
        Ok(entry.key.as_str())
    }

    /// Active profile's workspace id, if known. `None` is valid: a
    /// pasted `wk_` lands in a profile entry with `id = None` because
    /// the server has no "what workspace does this key belong to?"
    /// lookup.
    #[allow(dead_code)] // exposed for tests + future CLI integrations
    pub fn active_ws_id(&self) -> Option<&str> {
        let alias = self.active_alias()?;
        self.workspaces.get(alias)?.id.as_deref()
    }

    /// Replace (or insert) the active profile's entry. Used by init /
    /// login flows when the alias is implicit (DEFAULT_WORKSPACE_ALIAS).
    pub fn set_active_profile(&mut self, alias: &str, entry: WorkspaceEntry) {
        self.workspaces.insert(alias.to_string(), entry);
        self.active_workspace = Some(alias.to_string());
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
        assert!(cfg.active_workspace.is_none());
        assert!(cfg.workspaces.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips_new_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut saved = CliConfig {
            server_url: "http://example.com".into(),
            api_key: Some("vk-123".into()),
            ..CliConfig::default()
        };
        saved.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-uuid".into()),
                key: "wk-789".into(),
            },
        );
        saved.workspaces.insert(
            "work".into(),
            WorkspaceEntry {
                id: Some("ws-work".into()),
                key: "wk-work".into(),
            },
        );
        saved.save_to(&path).unwrap();

        let loaded = CliConfig::load_from(&path).unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.active_alias(), Some("default"));
        assert_eq!(loaded.active_wk().unwrap(), "wk-789");
        assert_eq!(loaded.active_ws_id(), Some("ws-uuid"));
    }

    #[test]
    fn legacy_top_level_workspace_fields_promote_to_default_profile() {
        // Simulates a config.toml from before multi-workspace: a single
        // workspace_id + workspace_key at the top level. Loading must
        // produce [workspaces.default] with active = default.
        let toml = r#"
server_url = "http://srv"
api_key = "vk-old"
workspace_id = "ws-legacy-id"
workspace_key = "wk-legacy-key"
"#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = CliConfig::load_from(&path).unwrap();

        assert_eq!(cfg.active_alias(), Some("default"));
        let def = cfg.workspace_for("default").unwrap();
        assert_eq!(def.id.as_deref(), Some("ws-legacy-id"));
        assert_eq!(def.key, "wk-legacy-key");
        assert_eq!(cfg.api_key.as_deref(), Some("vk-old"));
    }

    #[test]
    fn legacy_paste_wk_only_promotes_with_id_none() {
        // `veda login --api-key wk_…` legacy state: workspace_key set
        // but workspace_id not (because the server can't tell us which
        // workspace the key belongs to). Promotion must preserve that
        // — the new profile gets id=None, which `active_ws_id()`
        // surfaces as "(id unknown)" in status output.
        let toml = r#"
server_url = "http://srv"
workspace_key = "wk-orphan"
"#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = CliConfig::load_from(&path).unwrap();

        let def = cfg.workspace_for("default").unwrap();
        assert!(def.id.is_none(), "id must be None when legacy file had no workspace_id");
        assert_eq!(def.key, "wk-orphan");
        assert_eq!(cfg.active_wk().unwrap(), "wk-orphan");
        assert_eq!(cfg.active_ws_id(), None);
    }

    #[test]
    fn legacy_migration_then_save_strips_legacy_fields() {
        // Round-trip: legacy in, new shape out. The next load must
        // succeed without seeing the old top-level keys at all
        // (otherwise we'd have two sources of truth diverging over
        // time). The serialised TOML must not mention workspace_id or
        // workspace_key as top-level keys.
        let legacy = r#"
server_url = "http://srv"
api_key = "vk-1"
workspace_id = "ws-1"
workspace_key = "wk-1"
"#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, legacy).unwrap();
        let cfg = CliConfig::load_from(&path).unwrap();
        cfg.save_to(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[workspaces.default]"), "raw: {raw}");
        // The strings appear inside the table (id/key field names), but
        // never as top-level `workspace_id = …` / `workspace_key = …`
        // assignments. Match the assignment form to be precise.
        assert!(!raw.lines().any(|l| l.starts_with("workspace_id =")), "raw: {raw}");
        assert!(!raw.lines().any(|l| l.starts_with("workspace_key =")), "raw: {raw}");
    }

    #[test]
    fn new_format_takes_precedence_when_both_shapes_present() {
        // If a user hand-edited their config and put both a top-level
        // workspace_key AND a [workspaces.default] entry, the explicit
        // table is the source of truth — silently overwriting it would
        // lose work. (Documented in from_raw.)
        let toml = r#"
server_url = "http://srv"
workspace_id = "ws-legacy"
workspace_key = "wk-legacy"

[workspaces.default]
id = "ws-new"
key = "wk-new"
"#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = CliConfig::load_from(&path).unwrap();

        let def = cfg.workspace_for("default").unwrap();
        assert_eq!(def.id.as_deref(), Some("ws-new"));
        assert_eq!(def.key, "wk-new");
    }

    #[test]
    fn mixed_shape_without_default_does_not_promote_legacy() {
        // Edge case caught in Codex review: a hand-edited file with
        // [workspaces.work] (no default) AND a top-level
        // workspace_key. The earlier migration eagerly promoted the
        // legacy fields into a phantom `default` and even auto-set
        // `active_workspace = default` when the user hadn't picked
        // one — silently activating a workspace that no longer
        // matched what they had configured. The new rule is: any
        // new-shape map at all is canonical, legacy is ignored.
        let toml = r#"
server_url = "http://srv"
workspace_id = "ws-legacy"
workspace_key = "wk-legacy"

[workspaces.work]
id = "ws-work"
key = "wk-work"
"#;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = CliConfig::load_from(&path).unwrap();

        // No phantom `default` injected.
        assert!(
            !cfg.workspaces.contains_key("default"),
            "phantom default profile leaked: {:?}",
            cfg.workspaces
        );
        // The new-shape `work` profile is intact.
        let work = cfg.workspace_for("work").unwrap();
        assert_eq!(work.id.as_deref(), Some("ws-work"));
        // No `active_workspace` value is invented when the file didn't
        // declare one — caller has to pick explicitly.
        assert!(cfg.active_workspace.is_none());
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        CliConfig {
            server_url: default_server_url(),
            ..CliConfig::default()
        }
        .save_to(&path)
        .unwrap();
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        CliConfig {
            server_url: default_server_url(),
            ..CliConfig::default()
        }
        .save_to(&path)
        .unwrap();
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
    fn active_wk_errors_when_no_workspace_selected() {
        let cfg = CliConfig::default();
        let err = cfg.active_wk().unwrap_err().to_string();
        assert!(err.contains("veda init"), "msg: {err}");
    }

    #[test]
    fn active_wk_errors_when_alias_missing_from_map() {
        // Dangling active_workspace (alias points at a profile that
        // was removed) must give a friendly error, not a panic or
        // unwrap. Hand-edit / partial-write recovery path.
        let mut cfg = CliConfig::default();
        cfg.active_workspace = Some("ghost".into());
        let err = cfg.active_wk().unwrap_err().to_string();
        assert!(err.contains("ghost"), "msg: {err}");
        assert!(err.contains("veda workspace list"), "msg: {err}");
    }

    #[test]
    fn active_wk_errors_when_key_is_empty_placeholder() {
        // run_init writes a profile with `key: ""` before the
        // mint-key call so a mint failure can be retried — but the
        // placeholder must not be treated as a usable key by data
        // commands. Helper has to error locally with an executable
        // retry hint instead of forwarding the empty string as a
        // Bearer.
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            DEFAULT_WORKSPACE_ALIAS,
            WorkspaceEntry {
                id: Some("ws-incomplete".into()),
                key: String::new(),
            },
        );
        let err = cfg.active_wk().unwrap_err().to_string();
        assert!(err.contains("no key"), "msg: {err}");
        // The retry hint must be a single executable command the
        // user can paste. `veda workspace add default` (without
        // --workspace-id) hits the repair branch when there's a
        // stored id — verified by workspace::run_workspace_add tests.
        assert!(err.contains("veda workspace add default"), "msg: {err}");
        // Double-check: no `--workspace-id` clause leaks into the
        // hint (it would dispatch into the disagree-id branch and
        // confuse users who don't actually have a different id).
        assert!(!err.contains("--workspace-id"), "msg: {err}");
    }

    #[test]
    fn active_wk_errors_when_key_empty_and_id_unknown_points_at_init() {
        // Edge case: id is None as well as key — this shouldn't
        // happen in normal flows (paste-wk_ always sets a key) but
        // a hand-edited config could produce it. The hint must
        // route to `veda init` because `workspace add <alias>`
        // would bail (alias exists but repair has no stored id to
        // mint against).
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            DEFAULT_WORKSPACE_ALIAS,
            WorkspaceEntry { id: None, key: String::new() },
        );
        let err = cfg.active_wk().unwrap_err().to_string();
        assert!(err.contains("veda init"), "msg: {err}");
    }
}

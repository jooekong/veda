//! `veda status` — render current config + server reachability.
//!
//! The renderer is a pure function over `CliConfig` + `Option<bool>`
//! (server reachable / unreachable / not-checked) so tests don't need
//! a mock HTTP server to verify the layout.

use std::time::Duration;

use crate::config::CliConfig;

/// Server-reachability check: GET /healthz with a short timeout. Returns
/// true on 200 OK with body "ok", false on anything else (including
/// timeout, DNS failure, non-2xx, or unexpected body). Deliberately
/// permissive — the goal is "is the server answering" not "is it
/// fully healthy" (use /v1/ready for the latter).
pub async fn ping_server(server_url: &str) -> bool {
    let url = format!("{}/healthz", server_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => body.trim() == "ok",
            Err(_) => false,
        },
        _ => false,
    }
}

/// Format the status block. `reachable` = `Some(true)` for confirmed
/// reachable, `Some(false)` for confirmed unreachable, `None` for "did
/// not check" (e.g. config is empty so there's no server URL worth
/// checking).
pub fn render_status(cfg: &CliConfig, reachable: Option<bool>) -> String {
    if !is_configured(cfg) {
        return "No configuration. Run `veda init` to set up.\n".to_string();
    }
    let server_line = match reachable {
        Some(true) => format!("Server:    {}  (reachable)", cfg.server_url),
        Some(false) => format!("Server:    {}  (unreachable)", cfg.server_url),
        None => format!("Server:    {}", cfg.server_url),
    };
    let api_key_state = if cfg.api_key.as_deref().is_some_and(|s| !s.is_empty()) {
        "✓ configured"
    } else {
        "✗ missing"
    };
    let ws_state = render_workspace_line(cfg);
    format!(
        "{server_line}\nAPI key:   {api_key_state}\nWorkspace: {ws_state}\n"
    )
}

/// "Workspace:" line content. ★ marks the active profile so users
/// reading status alongside `workspace list` see the same marker.
/// When more than one profile exists, also count them — alerts users
/// who forgot they had a second profile sitting in the config.
fn render_workspace_line(cfg: &CliConfig) -> String {
    let active = match cfg.active_alias() {
        Some(a) => a,
        None => return "✗ none selected".to_string(),
    };
    let Some(entry) = cfg.workspaces.get(active) else {
        // Dangling active_workspace: profile was removed but the
        // active alias still points at it. Friendly nudge instead of
        // a vague error from the next command.
        return format!(
            "✗ active alias '{active}' missing from config — run `veda workspace switch <alias>`"
        );
    };
    let id_part = match entry.id.as_deref() {
        Some(id) => format!("({})", short_id(id)),
        None => "(id unknown)".into(),
    };
    let key_warning = if entry.key.is_empty() {
        // Mint-key failure marker — see config::active_wk()'s comment
        // about the placeholder profile written before the mint call.
        " (key missing — rerun `veda init` or `veda workspace add`)"
    } else {
        ""
    };
    let extras = if cfg.workspaces.len() > 1 {
        format!("   [{} profiles configured]", cfg.workspaces.len())
    } else {
        String::new()
    };
    format!("★ {active} {id_part}{key_warning}{extras}")
}

/// Truncate a UUID-shaped id to its first segment so the status line
/// stays one terminal line wide. Full id is still in `config.toml` /
/// `workspace list`.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// "Configured" = server_url is non-empty AND there's some auth state
/// to talk about: an account key, a workspace profile, or even a
/// stale `active_workspace` alias (so dangling state still surfaces
/// in the output rather than vanishing behind "No configuration").
fn is_configured(cfg: &CliConfig) -> bool {
    !cfg.server_url.is_empty()
        && (cfg.api_key.is_some()
            || !cfg.workspaces.is_empty()
            || cfg.active_workspace.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WorkspaceEntry, DEFAULT_WORKSPACE_ALIAS};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn full_cfg() -> CliConfig {
        let mut cfg = CliConfig {
            server_url: "http://example.com:3000".into(),
            api_key: Some("ak-123".into()),
            ..CliConfig::default()
        };
        cfg.set_active_profile(
            DEFAULT_WORKSPACE_ALIAS,
            WorkspaceEntry {
                // UUID shape — short_id() truncates to "0c022809" (first
                // hyphen-separated segment) so the status line fits one
                // terminal width even with a long UUID.
                id: Some("0c022809-f4f5-43db-97e2-49de83256d6d".into()),
                key: "wk-xyz".into(),
            },
        );
        cfg
    }

    #[test]
    fn render_unconfigured() {
        let out = render_status(&CliConfig::default(), None);
        assert!(out.contains("No configuration"), "out: {out}");
        assert!(out.contains("veda init"), "out: {out}");
    }

    #[test]
    fn render_full_with_reachable() {
        let out = render_status(&full_cfg(), Some(true));
        assert!(out.contains("http://example.com:3000"));
        assert!(out.contains("(reachable)"));
        assert!(out.contains("✓ configured"));
        // Active profile is marked with ★ and shows the short id
        // (first hyphen-separated segment) so the line stays short.
        assert!(out.contains("★ default"), "out: {out}");
        assert!(out.contains("(0c022809)"), "expected short id, out: {out}");
        // Make sure we don't leak secrets.
        assert!(!out.contains("ak-123"), "API key leaked: {out}");
        assert!(!out.contains("wk-xyz"), "WS key leaked: {out}");
    }

    #[test]
    fn render_full_with_unreachable() {
        let out = render_status(&full_cfg(), Some(false));
        assert!(out.contains("(unreachable)"));
    }

    #[test]
    fn render_no_check_omits_reachability() {
        let out = render_status(&full_cfg(), None);
        assert!(!out.contains("reachable"), "out: {out}");
        assert!(!out.contains("unreachable"), "out: {out}");
        // No trailing whitespace inside the Server line.
        assert!(
            !out.lines().any(|l| l.starts_with("Server:") && l.ends_with(' ')),
            "trailing whitespace on server line: {out:?}"
        );
    }

    #[test]
    fn render_dangling_active_alias_nudges_user() {
        // active_workspace points at a profile that's not in the map.
        // Hand-edit / partial-rm recovery: tell the user how to fix
        // it instead of just printing "(id unknown)".
        let cfg = CliConfig {
            api_key: Some("k".into()),
            active_workspace: Some("ghost".into()),
            ..CliConfig::default()
        };
        let out = render_status(&cfg, None);
        assert!(out.contains("ghost"), "out: {out}");
        assert!(out.contains("workspace switch"), "out: {out}");
    }

    #[test]
    fn render_api_key_only_no_workspace() {
        let cfg = CliConfig {
            api_key: Some("k".into()),
            ..CliConfig::default()
        };
        let out = render_status(&cfg, Some(true));
        assert!(out.contains("none selected"));
    }

    #[test]
    fn render_paste_wk_profile_renders_as_ready() {
        // Paste-a-wk_ flow lands in a [workspaces.default] entry with
        // id=None — status must surface that ("id unknown") without
        // calling it "none selected", which would imply the CLI is
        // unusable.
        let mut cfg = CliConfig {
            api_key: None,
            ..CliConfig::default()
        };
        cfg.set_active_profile(
            DEFAULT_WORKSPACE_ALIAS,
            WorkspaceEntry { id: None, key: "wk-x".into() },
        );
        let out = render_status(&cfg, Some(true));
        assert!(out.contains("★ default"), "out: {out}");
        assert!(out.contains("(id unknown)"), "out: {out}");
        assert!(!out.contains("none selected"), "out: {out}");
    }

    #[test]
    fn render_empty_key_placeholder_shows_warning() {
        // run_init's mint-key failure path writes id+empty-key. status
        // must call that out so the user notices the half-finished
        // onboarding, instead of just showing "★ default (id)" as if
        // everything worked.
        let mut cfg = CliConfig {
            api_key: Some("k".into()),
            ..CliConfig::default()
        };
        cfg.set_active_profile(
            DEFAULT_WORKSPACE_ALIAS,
            WorkspaceEntry {
                id: Some("ws-stuck".into()),
                key: String::new(),
            },
        );
        let out = render_status(&cfg, Some(true));
        assert!(out.contains("key missing"), "out: {out}");
    }

    #[test]
    fn render_multiple_profiles_shows_count() {
        // Hint when more than one profile is configured so users don't
        // forget about a stale `work` profile sitting in the config.
        let mut cfg = CliConfig {
            api_key: Some("k".into()),
            ..CliConfig::default()
        };
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-a".into()),
                key: "wk-a".into(),
            },
        );
        cfg.workspaces.insert(
            "work".into(),
            WorkspaceEntry {
                id: Some("ws-b".into()),
                key: "wk-b".into(),
            },
        );
        let out = render_status(&cfg, None);
        assert!(out.contains("★ default"), "out: {out}");
        assert!(out.contains("2 profiles"), "out: {out}");
    }

    #[tokio::test]
    async fn ping_returns_true_on_200_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        assert!(ping_server(&server.uri()).await);
    }

    #[tokio::test]
    async fn ping_returns_false_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(!ping_server(&server.uri()).await);
    }

    #[tokio::test]
    async fn ping_returns_false_on_unexpected_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
            .mount(&server)
            .await;

        assert!(!ping_server(&server.uri()).await);
    }

    #[tokio::test]
    async fn ping_returns_false_on_unreachable_host() {
        // Reserved-for-doc port that nothing is listening on. Connect-time
        // 2s timeout fires fast.
        assert!(!ping_server("http://127.0.0.1:1").await);
    }
}

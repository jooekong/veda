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
    let ws_state = match (&cfg.workspace_id, &cfg.workspace_key) {
        (Some(id), Some(_)) => format!("✓ {id}"),
        (Some(id), None) => format!("✗ {id} (no workspace key — re-run `veda workspace use {id}`)"),
        // wk_*-only paste: the workspace key alone is enough to run
        // data commands; the id is unknown because the server has no
        // "what workspace does this key belong to" endpoint.
        (None, Some(_)) => "✓ workspace key configured (id unknown)".to_string(),
        (None, None) => "✗ none selected".to_string(),
    };
    format!(
        "{server_line}\nAPI key:   {api_key_state}\nWorkspace: {ws_state}\n"
    )
}

/// "Configured" = server_url is non-empty AND at least one of the
/// auth fields is set. A bare `server_url = "http://localhost:3000"`
/// (the default) with no api_key counts as unconfigured — that's
/// what `CliConfig::default()` returns.
fn is_configured(cfg: &CliConfig) -> bool {
    !cfg.server_url.is_empty()
        && (cfg.api_key.is_some() || cfg.workspace_key.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn full_cfg() -> CliConfig {
        CliConfig {
            server_url: "http://example.com:3000".into(),
            api_key: Some("ak-123".into()),
            workspace_id: Some("ws-abc".into()),
            workspace_key: Some("wk-xyz".into()),
        }
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
        assert!(out.contains("ws-abc"));
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
    fn render_workspace_id_without_key_warns() {
        let cfg = CliConfig {
            api_key: Some("k".into()),
            workspace_id: Some("ws-abc".into()),
            workspace_key: None,
            ..CliConfig::default()
        };
        let out = render_status(&cfg, None);
        assert!(out.contains("ws-abc"));
        assert!(out.contains("re-run"), "out: {out}");
    }

    #[test]
    fn render_api_key_only_no_workspace() {
        let cfg = CliConfig {
            api_key: Some("k".into()),
            workspace_id: None,
            workspace_key: None,
            ..CliConfig::default()
        };
        let out = render_status(&cfg, Some(true));
        assert!(out.contains("none selected"));
    }

    #[test]
    fn render_workspace_key_only_renders_as_ready() {
        // Paste-a-wk_ shape: `veda login --api-key wk_xxx` clears
        // workspace_id (server has no lookup endpoint) and api_key.
        // Data commands still work, so status must not call this
        // "none selected" — that would mislead users into thinking
        // onboarding failed.
        let cfg = CliConfig {
            api_key: None,
            workspace_id: None,
            workspace_key: Some("wk-x".into()),
            ..CliConfig::default()
        };
        let out = render_status(&cfg, Some(true));
        assert!(out.contains("workspace key configured"), "out: {out}");
        assert!(out.contains("id unknown"), "out: {out}");
        assert!(!out.contains("none selected"), "out: {out}");
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

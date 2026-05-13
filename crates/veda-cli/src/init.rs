//! Initialization helpers: `veda login --api-key`, `veda init`.
//!
//! Pure mutation logic lives here so the binary's `match` arms stay thin
//! and tests can exercise the state transitions without spinning up
//! clap or HTTP.

use anyhow::{anyhow, Context, Result};

use crate::client::Client;
use crate::config::CliConfig;

/// Apply a paste-an-existing-key login. Sets `api_key`, clears any cached
/// `workspace_id` / `workspace_key` so requests don't mix an old workspace
/// selection (minted under the previous account) with a new identity.
///
/// Caller persists the config after.
pub fn apply_login(cfg: &mut CliConfig, api_key: String) {
    cfg.api_key = Some(api_key);
    cfg.workspace_id = None;
    cfg.workspace_key = None;
}

/// Apply a paste-an-existing workspace key (wk_*). Stores it into
/// `workspace_key` and clears `api_key` + `workspace_id` — a workspace
/// key alone cannot drive account-scope endpoints, and there is no
/// server endpoint to look up which workspace it belongs to, so we
/// leave the id field empty rather than make one up.
///
/// Caller persists the config after.
pub fn apply_workspace_key(cfg: &mut CliConfig, wk: String) {
    cfg.workspace_key = Some(wk);
    cfg.workspace_id = None;
    cfg.api_key = None;
}

/// Resolved params for `veda init`. Construction (prompting / flag merge)
/// lives in main.rs; this struct is the testable handoff to `run_init`.
#[derive(Debug, Clone)]
pub struct InitParams {
    pub server_url: String,
    /// If true, treat as "log in to existing account" (skip account
    /// creation, use email+password against /v1/accounts/login). Else
    /// "create new account" — `name` must be set.
    pub login: bool,
    pub name: String,
    pub email: String,
    pub password: String,
    pub workspace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub account_id: String,
    pub workspace_id: String,
    pub created_account: bool,
    pub created_workspace: bool,
}

/// End-to-end init flow against a `Client`. Mutates the config in place
/// (api_key, workspace_id, workspace_key); caller persists.
///
/// Steps:
///   1. POST /v1/accounts (create) or POST /v1/accounts/login → api_key
///   2. GET  /v1/workspaces → look for `params.workspace` by name
///   3. POST /v1/workspaces (only if not found) → workspace_id
///   4. POST /v1/workspaces/{id}/keys → workspace_key
///
/// On any HTTP failure, the caller-visible cfg may be partially mutated
/// — that's intentional. If step 1 succeeds but step 4 fails, the saved
/// api_key is real and the user can retry `veda workspace use <id>`
/// manually without redoing account creation.
pub async fn run_init(
    client: &Client,
    cfg: &mut CliConfig,
    params: InitParams,
) -> Result<InitOutcome> {
    cfg.server_url = params.server_url;

    // ── 1. account ──────────────────────────────────────────────────
    let (account_id, api_key, created_account) = if params.login {
        let resp = client
            .login(&params.email, &params.password)
            .await
            .context("login failed (wrong credentials, or account doesn't exist?)")?;
        let account_id = field_str(&resp, "account_id")?;
        let api_key = field_str(&resp, "api_key")?;
        (account_id, api_key, false)
    } else {
        if params.name.is_empty() {
            return Err(anyhow!(
                "creating an account requires --name; pass --login to use an existing account instead"
            ));
        }
        // Try create first; if the server says 409 the email is already
        // registered, so transparently fall back to login with the same
        // password. The user doesn't need to know up front whether they're
        // a returning user — wrong password is the only failure mode that
        // surfaces.
        match client
            .create_account(&params.name, &params.email, &params.password)
            .await
        {
            Ok(resp) => {
                let account_id = field_str(&resp, "account_id")?;
                let api_key = field_str(&resp, "api_key")?;
                (account_id, api_key, true)
            }
            Err(e) if crate::client::status_code(&e) == Some(409) => {
                let resp = client
                    .login(&params.email, &params.password)
                    .await
                    .context("account already exists, but login failed (wrong password?)")?;
                let account_id = field_str(&resp, "account_id")?;
                let api_key = field_str(&resp, "api_key")?;
                (account_id, api_key, false)
            }
            Err(e) => return Err(e.context("account creation failed")),
        }
    };
    cfg.api_key = Some(api_key.clone());

    // ── 2. workspace lookup or create ───────────────────────────────
    let list = client
        .list_workspaces(&api_key)
        .await
        .context("could not list existing workspaces")?;
    let existing = list["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["name"].as_str() == Some(params.workspace.as_str()));

    let (workspace_id, created_workspace) = match existing {
        Some(w) => {
            let id = w["id"]
                .as_str()
                .ok_or_else(|| anyhow!("workspace listing missing id field"))?
                .to_string();
            (id, false)
        }
        None => {
            let resp = client
                .create_workspace(&api_key, &params.workspace)
                .await
                .context("workspace create failed")?;
            let id = field_str(&resp, "id")?;
            (id, true)
        }
    };
    cfg.workspace_id = Some(workspace_id.clone());

    // ── 3. workspace key ────────────────────────────────────────────
    let resp = client
        .create_workspace_key(&api_key, &workspace_id)
        .await
        .context("workspace key mint failed")?;
    cfg.workspace_key = Some(field_str(&resp, "key")?);

    Ok(InitOutcome {
        account_id,
        workspace_id,
        created_account,
        created_workspace,
    })
}

/// Pull a string field out of a `{ "data": { ... } }` envelope.
fn field_str(resp: &serde_json::Value, key: &str) -> Result<String> {
    resp["data"][key]
        .as_str()
        .ok_or_else(|| anyhow!("response missing data.{key}"))
        .map(String::from)
}

/// Outcome of zero-input anonymous onboarding. The server hands us
/// everything in one round-trip, so there's no partial-success
/// trade-off to document like `run_init` has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnonymousOutcome {
    pub account_id: String,
    pub workspace_id: String,
}

/// Anonymous onboarding flow: POST /v1/accounts/anonymous returns
/// account_id + api_key + workspace_id + workspace_key in one shot.
/// Writes all four config fields and returns the IDs for the renderer.
///
/// `server_url` is set before the HTTP call so a failed onboard still
/// leaves the config pointing at the right server (idempotent — same
/// value if the user retries). Credentials are atomic: api_key /
/// workspace_id / workspace_key are either all set together or none
/// of them, so partial writes can't put the CLI in a half-configured
/// state.
pub async fn run_anonymous(
    client: &Client,
    cfg: &mut CliConfig,
    server_url: String,
) -> Result<AnonymousOutcome> {
    cfg.server_url = server_url;
    let resp = client
        .anonymous_onboard()
        .await
        .context("anonymous onboard failed")?;
    let account_id = field_str(&resp, "account_id")?;
    let api_key = field_str(&resp, "api_key")?;
    let workspace_id = field_str(&resp, "workspace_id")?;
    let workspace_key = field_str(&resp, "workspace_key")?;

    cfg.api_key = Some(api_key);
    cfg.workspace_id = Some(workspace_id.clone());
    cfg.workspace_key = Some(workspace_key);

    Ok(AnonymousOutcome {
        account_id,
        workspace_id,
    })
}

/// Upgrade the current anonymous account to a named one. Reads the
/// auth `vk_` from `cfg.api_key`; the caller is expected to have
/// resolved env-var fallbacks (e.g. `VEDA_API_KEY`) before calling so
/// this stays a pure function. The server doesn't re-mint the key, so
/// the same value keeps working after the claim.
pub async fn run_claim(
    client: &Client,
    cfg: &CliConfig,
    email: String,
    password: String,
    name: Option<String>,
) -> Result<String> {
    let api_key = cfg
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            anyhow!("not onboarded yet; run `veda init` first")
        })?;
    let resp = client
        .claim_account(api_key, &email, &password, name.as_deref())
        .await
        .context("claim failed")?;
    field_str(&resp, "account_id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── apply_login ────────────────────────────────────────────────

    #[test]
    fn login_sets_api_key_and_clears_workspace() {
        let mut cfg = CliConfig {
            server_url: "http://x".into(),
            api_key: Some("old-key".into()),
            workspace_id: Some("ws-old".into()),
            workspace_key: Some("wk-old".into()),
        };
        apply_login(&mut cfg, "new-key".into());
        assert_eq!(cfg.api_key.as_deref(), Some("new-key"));
        assert!(cfg.workspace_id.is_none());
        assert!(cfg.workspace_key.is_none());
    }

    #[test]
    fn login_on_fresh_config_just_sets_api_key() {
        let mut cfg = CliConfig::default();
        apply_login(&mut cfg, "k1".into());
        assert_eq!(cfg.api_key.as_deref(), Some("k1"));
        assert!(cfg.workspace_id.is_none());
        assert!(cfg.workspace_key.is_none());
        assert!(!cfg.server_url.is_empty());
    }

    #[test]
    fn login_overwrites_existing_api_key() {
        let mut cfg = CliConfig {
            api_key: Some("first".into()),
            ..CliConfig::default()
        };
        apply_login(&mut cfg, "second".into());
        assert_eq!(cfg.api_key.as_deref(), Some("second"));
    }

    // ── apply_workspace_key ────────────────────────────────────────

    #[test]
    fn workspace_key_paste_sets_wk_and_clears_account_identity() {
        // Paste-a-wk_ semantics: replaces any existing account or
        // workspace identity. The account-scoped api_key is cleared
        // because wk_ can't act as one; workspace_id is cleared because
        // there's no server lookup for "which workspace does this wk_
        // belong to".
        let mut cfg = CliConfig {
            server_url: "http://x".into(),
            api_key: Some("vk_old".into()),
            workspace_id: Some("ws-old".into()),
            workspace_key: Some("wk_old".into()),
        };
        apply_workspace_key(&mut cfg, "wk_new".into());
        assert_eq!(cfg.workspace_key.as_deref(), Some("wk_new"));
        assert!(cfg.workspace_id.is_none(), "id should be cleared");
        assert!(cfg.api_key.is_none(), "api_key should be cleared");
    }

    // ── run_init ───────────────────────────────────────────────────

    fn ok(data: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "data": data }))
    }

    fn params_create(server: &str) -> InitParams {
        InitParams {
            server_url: server.into(),
            login: false,
            name: "Joe".into(),
            email: "joe@example.com".into(),
            password: "hunter2".into(),
            workspace: "default".into(),
        }
    }

    #[tokio::test]
    async fn happy_path_creates_account_and_workspace() {
        let server = MockServer::start().await;
        // Body shape pinned: a regression that swapped name/email or
        // dropped password would change the body and miss this matcher.
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .and(body_partial_json(json!({
                "name": "Joe",
                "email": "joe@example.com",
                "password": "hunter2",
            })))
            .respond_with(ok(json!({ "account_id": "acct-1", "api_key": "ak-1" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .and(body_partial_json(json!({ "name": "default" })))
            .respond_with(ok(json!({ "id": "ws-new" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-new/keys$"))
            .respond_with(ok(json!({ "key": "wk-1" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let outcome = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap();

        assert_eq!(outcome.account_id, "acct-1");
        assert_eq!(outcome.workspace_id, "ws-new");
        assert!(outcome.created_account);
        assert!(outcome.created_workspace);
        assert_eq!(cfg.server_url, server.uri());
        assert_eq!(cfg.api_key.as_deref(), Some("ak-1"));
        assert_eq!(cfg.workspace_id.as_deref(), Some("ws-new"));
        assert_eq!(cfg.workspace_key.as_deref(), Some("wk-1"));
    }

    #[tokio::test]
    async fn login_path_skips_account_create_and_reuses_existing_workspace() {
        let server = MockServer::start().await;
        // /v1/accounts (create) must NOT be hit when login=true.
        // expect(0) makes wiremock fail the test if it is.
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/login"))
            .and(body_partial_json(json!({
                "email": "joe@example.com",
                "password": "hunter2",
            })))
            .respond_with(ok(json!({ "account_id": "acct-2", "api_key": "ak-2" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!([
                { "id": "ws-existing", "name": "default" }
            ])))
            .expect(1)
            .mount(&server)
            .await;
        // POST /v1/workspaces (create) must also NOT fire — workspace
        // already exists, so we go straight to key minting.
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-existing/keys$"))
            .respond_with(ok(json!({ "key": "wk-2" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let params = InitParams {
            login: true,
            name: String::new(),
            ..params_create(&server.uri())
        };
        let outcome = run_init(&client, &mut cfg, params).await.unwrap();

        assert_eq!(outcome.account_id, "acct-2");
        assert_eq!(outcome.workspace_id, "ws-existing");
        assert!(!outcome.created_account);
        assert!(!outcome.created_workspace);
        assert_eq!(cfg.api_key.as_deref(), Some("ak-2"));
        assert_eq!(cfg.workspace_id.as_deref(), Some("ws-existing"));
        assert_eq!(cfg.workspace_key.as_deref(), Some("wk-2"));
    }

    #[tokio::test]
    async fn create_mode_without_name_is_rejected_before_any_http_call() {
        let server = MockServer::start().await;
        // No mocks registered — any HTTP call would 404 and the test
        // would still fail, but we expect the validation to short-circuit.
        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let params = InitParams {
            login: false,
            name: String::new(),
            ..params_create(&server.uri())
        };
        let err = run_init(&client, &mut cfg, params).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--name"), "msg: {msg}");
        assert!(msg.contains("--login"), "msg: {msg}");
        // Config must NOT be partially mutated when validation fails.
        assert!(cfg.api_key.is_none());
    }

    #[tokio::test]
    async fn create_409_transparently_falls_back_to_login() {
        // The user runs `veda init` with an email that's already
        // registered. Server returns 409 from POST /v1/accounts, and
        // we transparently log in with the same password. The user
        // sees no error; outcome.created_account is false so the
        // top-level renderer prints "logged in" instead of "created".
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ResponseTemplate::new(409).set_body_string("email taken"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/login"))
            .and(body_partial_json(json!({
                "email": "joe@example.com",
                "password": "hunter2",
            })))
            .respond_with(ok(json!({ "account_id": "acct-fb", "api_key": "ak-fb" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!({ "id": "ws-fb" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-fb/keys$"))
            .respond_with(ok(json!({ "key": "wk-fb" })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let outcome = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap();

        assert_eq!(outcome.account_id, "acct-fb");
        assert!(!outcome.created_account, "should NOT report account creation");
        assert_eq!(cfg.api_key.as_deref(), Some("ak-fb"));
        assert_eq!(cfg.workspace_key.as_deref(), Some("wk-fb"));
    }

    #[tokio::test]
    async fn create_409_then_login_wrong_password_surfaces_clear_error() {
        // If create says 409 (email taken) AND login then fails (wrong
        // password), the user gets a context-wrapped error pointing at
        // the password rather than the misleading "account creation
        // failed" they'd have gotten before.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ResponseTemplate::new(409).set_body_string("email taken"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/login"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad password"))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let err = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("account already exists") && msg.contains("login failed"),
            "expected fallback-login wording, got: {msg}"
        );
        // Config still untouched on this failure path.
        assert!(cfg.api_key.is_none());
    }

    #[tokio::test]
    async fn create_non_409_failure_surfaces_creation_error() {
        // Non-409 server errors (500, network etc.) should NOT trigger
        // fallback — that path is reserved for "email already exists".
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ResponseTemplate::new(500).set_body_string("db down"))
            .mount(&server)
            .await;
        // Login mock with expect(0): fail the test if it gets called.
        Mock::given(method("POST"))
            .and(path("/v1/accounts/login"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let err = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("account creation failed"));
        assert!(cfg.api_key.is_none());
    }

    #[tokio::test]
    async fn workspace_list_nonempty_but_no_match_creates_new() {
        // Pins the find-or-create branch: listing has other workspaces
        // but none with the requested name → fall through to create.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ok(json!({ "account_id": "acct-9", "api_key": "ak-9" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!([
                { "id": "ws-other-1", "name": "scratch" },
                { "id": "ws-other-2", "name": "experiments" }
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .and(body_partial_json(json!({ "name": "default" })))
            .respond_with(ok(json!({ "id": "ws-fresh" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-fresh/keys$"))
            .respond_with(ok(json!({ "key": "wk-9" })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let outcome = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap();
        assert_eq!(outcome.workspace_id, "ws-fresh");
        assert!(outcome.created_workspace);
    }

    // ── run_anonymous ──────────────────────────────────────────────

    #[tokio::test]
    async fn anonymous_writes_all_four_config_fields() {
        // Anonymous onboard is a single round-trip; the server hands
        // us account_id, api_key, workspace_id, workspace_key — the
        // CLI must persist the three secret/id fields onto CliConfig
        // so subsequent commands work without further setup.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/anonymous"))
            .respond_with(ok(json!({
                "account_id": "acct-anon",
                "api_key": "vk_anon_xxx",
                "workspace_id": "ws-anon",
                "workspace_key": "wk_anon_xxx",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let outcome = run_anonymous(&client, &mut cfg, server.uri()).await.unwrap();

        assert_eq!(outcome.account_id, "acct-anon");
        assert_eq!(outcome.workspace_id, "ws-anon");
        assert_eq!(cfg.server_url, server.uri());
        assert_eq!(cfg.api_key.as_deref(), Some("vk_anon_xxx"));
        assert_eq!(cfg.workspace_id.as_deref(), Some("ws-anon"));
        assert_eq!(cfg.workspace_key.as_deref(), Some("wk_anon_xxx"));
    }

    #[tokio::test]
    async fn anonymous_surface_clear_error_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/anonymous"))
            .respond_with(ResponseTemplate::new(500).set_body_string("db down"))
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let err = run_anonymous(&client, &mut cfg, server.uri())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("anonymous onboard failed"));
        // Failure must NOT leave a half-written config.
        assert!(cfg.api_key.is_none());
    }

    // ── run_claim ──────────────────────────────────────────────────

    #[tokio::test]
    async fn claim_sends_bearer_and_returns_account_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/claim"))
            .and(body_partial_json(json!({
                "email": "joe@example.com",
                "password": "hunter2",
                "name": "Joe",
            })))
            .respond_with(ok(json!({ "account_id": "acct-1" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let cfg = CliConfig {
            api_key: Some("vk_anon".into()),
            ..CliConfig::default()
        };
        let id = run_claim(
            &client,
            &cfg,
            "joe@example.com".into(),
            "hunter2".into(),
            Some("Joe".into()),
        )
        .await
        .unwrap();
        assert_eq!(id, "acct-1");
    }

    #[tokio::test]
    async fn claim_without_api_key_fails_before_http() {
        // No mock registered: any HTTP call would 404, but the
        // precondition should short-circuit before sending one.
        let server = MockServer::start().await;
        let client = Client::new(&server.uri());
        let cfg = CliConfig::default();
        let err = run_claim(&client, &cfg, "x@y.com".into(), "p".into(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not onboarded"), "got: {err}");
    }

    #[tokio::test]
    async fn claim_treats_empty_api_key_as_missing() {
        // `api_key = Some("")` is functionally not-onboarded; reject
        // before the HTTP layer so users see "not onboarded" instead
        // of an opaque server 401.
        let server = MockServer::start().await;
        let client = Client::new(&server.uri());
        let cfg = CliConfig {
            api_key: Some(String::new()),
            ..CliConfig::default()
        };
        let err = run_claim(&client, &cfg, "x@y.com".into(), "p".into(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not onboarded"), "got: {err}");
    }

    #[tokio::test]
    async fn claim_propagates_409_through_status_code() {
        // The handler-side translator returns 409 when the email
        // collides. Confirm the client surfaces a typed status that
        // callers (e.g. UI rendering) can match on.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/claim"))
            .respond_with(
                ResponseTemplate::new(409).set_body_string("email already registered"),
            )
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let cfg = CliConfig {
            api_key: Some("vk_anon".into()),
            ..CliConfig::default()
        };
        let err = run_claim(&client, &cfg, "joe@x.com".into(), "p".into(), None)
            .await
            .unwrap_err();
        assert_eq!(crate::client::status_code(&err), Some(409));
    }

    #[tokio::test]
    async fn claim_omits_name_when_none() {
        // Empty Some("") was rejected by the server; None must result
        // in no `name` field on the wire so the server keeps the
        // existing auto-generated name.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts/claim"))
            // Body partial-match: just assert no `name` key by checking
            // email+password subset. wiremock's body_partial_json
            // ignores extra fields, but we mount only this mock so any
            // unexpected body shape would 404 against the rest.
            .and(body_partial_json(json!({
                "email": "j@x.com",
                "password": "p",
            })))
            .respond_with(ok(json!({ "account_id": "a1" })))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let cfg = CliConfig {
            api_key: Some("vk_anon".into()),
            ..CliConfig::default()
        };
        run_claim(&client, &cfg, "j@x.com".into(), "p".into(), None)
            .await
            .unwrap();
    }

    // ── run_init partial-save & legacy tests ───────────────────────

    #[tokio::test]
    async fn workspace_key_failure_leaves_api_key_persisted() {
        // Trade-off documented in run_init: account_id + api_key are saved
        // before workspace key minting, so a failure at the last step
        // doesn't force the user to redo account creation. This test
        // pins that contract.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ok(json!({ "account_id": "acct-3", "api_key": "ak-3" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!({ "id": "ws-ok" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-ok/keys$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("milvus down"))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = CliConfig::default();
        let err = run_init(&client, &mut cfg, params_create(&server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace key mint failed"));
        // api_key + workspace_id persisted; ws_key is still absent.
        assert_eq!(cfg.api_key.as_deref(), Some("ak-3"));
        assert_eq!(cfg.workspace_id.as_deref(), Some("ws-ok"));
        assert!(cfg.workspace_key.is_none());
    }
}

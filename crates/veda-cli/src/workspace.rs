//! Workspace profile management: `veda workspace add / switch / list / rm`.
//!
//! Each public function mutates `CliConfig` and returns a short
//! summary string the binary's match arm prints. Splitting this out of
//! main.rs keeps the clap glue thin and lets tests drive the HTTP
//! interactions against `wiremock` without spinning up clap.

use anyhow::{bail, Context, Result};

use crate::client::Client;
use crate::config::{CliConfig, WorkspaceEntry};

/// Outcome of `workspace add`. `auto_switched` is true when the new
/// alias became the active workspace because there was no other one
/// configured. `repaired` is true when the alias already existed
/// locally with an empty key (half-finished onboarding) and this
/// call filled in the key instead of creating a fresh workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub alias: String,
    pub workspace_id: String,
    pub auto_switched: bool,
    pub repaired: bool,
}

/// `veda workspace add <alias> [--workspace-id <id>]`.
///
/// Three paths:
///   1. **Fresh add**: alias unknown, `workspace_id` None → server
///      creates a new workspace named after the alias, then we mint
///      a key.
///   2. **Existing-server add**: alias unknown, `workspace_id` Some
///      → mint a key against that id (no create).
///   3. **Repair**: alias already exists locally but its key is
///      empty (mint-key step of a previous onboarding failed) →
///      skip create, mint key against the stored id, overwrite the
///      placeholder. This is what `active_wk()`'s retry hint
///      sends users at.
///
/// The id portion of the local profile is written **before** the
/// mint-key call (path 1) so a failure there still leaves a
/// recoverable state on disk: `workspace add <alias>` retry hits
/// the repair branch above instead of trying to re-create a
/// server-side workspace and leaving the original as an orphan.
/// The caller is responsible for `cfg.save()` and should save even
/// on error so the placeholder lands on disk.
pub async fn run_workspace_add(
    client: &Client,
    cfg: &mut CliConfig,
    alias: String,
    workspace_id: Option<String>,
) -> Result<AddOutcome> {
    let api_key = cfg.api_key()?.to_string();
    // Snapshot before any path writes its placeholder — drives the
    // "first profile ever" auto-switch decision below.
    let was_empty_before = cfg.workspaces.is_empty();

    let repaired = match cfg.workspaces.get(&alias) {
        Some(e) if e.key.is_empty() => true,
        Some(_) => bail!(
            "workspace alias '{alias}' already exists. \
             Use `veda workspace rm {alias}` first to overwrite, \
             or `veda workspace switch {alias}` to use it."
        ),
        None => false,
    };

    let ws_id = if repaired {
        // Trust the stored id over any --workspace-id the user
        // passed — they'd otherwise abandon the server-side
        // workspace from the half-finished onboarding.
        let stored = cfg
            .workspaces
            .get(&alias)
            .and_then(|e| e.id.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("workspace '{alias}' has no stored id; cannot repair")
            })?;
        if let Some(id) = workspace_id {
            if id != stored {
                bail!(
                    "workspace '{alias}' is being repaired against existing id {stored}; \
                     --workspace-id {id} disagrees. Run `veda workspace rm {alias}` first \
                     if you really want a different workspace."
                );
            }
        }
        stored
    } else {
        match workspace_id {
            Some(id) => id,
            None => {
                let resp = client
                    .create_workspace(&api_key, &alias)
                    .await
                    .context("create workspace failed")?;
                let id = resp["data"]["id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("server response missing data.id"))?
                    .to_string();
                // Persist id immediately as an empty-key placeholder
                // so a mint failure below still gives users a way to
                // resume — the next `veda workspace add <alias>`
                // call takes the repair branch above.
                cfg.workspaces.insert(
                    alias.clone(),
                    WorkspaceEntry {
                        id: Some(id.clone()),
                        key: String::new(),
                    },
                );
                if was_empty_before {
                    cfg.active_workspace = Some(alias.clone());
                }
                id
            }
        }
    };

    let resp = client
        .create_workspace_key(&api_key, &ws_id)
        .await
        .context("mint workspace key failed")?;
    let wk = resp["data"]["key"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("server response missing data.key"))?
        .to_string();

    // Auto-switch only on a truly first-ever add: never on repair
    // (placeholder was already active), and never when other
    // profiles existed at the start of this call. `was_empty_before`
    // captures the state pre-placeholder, so it stays correct for
    // both the create-then-mint path (path 1, which wrote a
    // placeholder mid-flight) and the mint-only path (path 2).
    let auto_switched = !repaired && was_empty_before;
    cfg.workspaces.insert(
        alias.clone(),
        WorkspaceEntry {
            id: Some(ws_id.clone()),
            key: wk,
        },
    );
    if auto_switched {
        cfg.active_workspace = Some(alias.clone());
    }
    Ok(AddOutcome {
        alias,
        workspace_id: ws_id,
        auto_switched,
        repaired,
    })
}

/// `veda workspace switch <alias>`. Returns the previous active alias
/// (for the "switched: a → b" message). Errors when the alias is not
/// configured rather than silently planting a dangling pointer.
pub fn run_workspace_switch(cfg: &mut CliConfig, alias: String) -> Result<String> {
    cfg.workspace_for(&alias)?;
    let prev = cfg.active_alias().unwrap_or("<none>").to_string();
    cfg.active_workspace = Some(alias);
    Ok(prev)
}

/// `veda workspace rm <alias>`. Alias-only deletion — the server-side
/// wk_ stays valid because there's no revoke endpoint yet. Rejects
/// removing the active profile (user would lose every data command
/// until they switched).
pub fn run_workspace_rm(cfg: &mut CliConfig, alias: &str) -> Result<()> {
    if !cfg.workspaces.contains_key(alias) {
        bail!("no such workspace profile '{alias}'");
    }
    if cfg.active_alias() == Some(alias) {
        bail!(
            "'{alias}' is the active workspace. \
             Switch first: `veda workspace switch <other-alias>`"
        );
    }
    cfg.workspaces.remove(alias);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok(data: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "data": data }))
    }

    fn cfg_with_api_key() -> CliConfig {
        CliConfig {
            api_key: Some("vk-test".into()),
            ..CliConfig::default()
        }
    }

    // ── add ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_without_workspace_id_creates_workspace_and_mints_key() {
        // Server is hit twice: POST /v1/workspaces (create by name)
        // then POST /v1/workspaces/{id}/keys (mint wk_). Both bodies
        // are partially asserted so a regression that swapped the
        // alias for some other field would still fail.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .and(body_partial_json(json!({ "name": "scratch" })))
            .respond_with(ok(json!({ "id": "ws-scratch" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-scratch/keys$"))
            .respond_with(ok(json!({ "key": "wk-scratch" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        let out = run_workspace_add(&client, &mut cfg, "scratch".into(), None)
            .await
            .unwrap();

        assert_eq!(out.workspace_id, "ws-scratch");
        assert!(out.auto_switched, "first profile auto-switches");
        assert_eq!(cfg.active_alias(), Some("scratch"));
        let entry = cfg.workspace_for("scratch").unwrap();
        assert_eq!(entry.id.as_deref(), Some("ws-scratch"));
        assert_eq!(entry.key, "wk-scratch");
    }

    #[tokio::test]
    async fn add_with_workspace_id_skips_create_and_only_mints_key() {
        // When --workspace-id is given, the server-side workspace
        // already exists. POST /v1/workspaces (create) MUST NOT fire.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-existing/keys$"))
            .respond_with(ok(json!({ "key": "wk-shared" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        let out = run_workspace_add(
            &client,
            &mut cfg,
            "shared".into(),
            Some("ws-existing".into()),
        )
        .await
        .unwrap();
        assert_eq!(out.workspace_id, "ws-existing");
        assert_eq!(cfg.workspace_for("shared").unwrap().key, "wk-shared");
    }

    // ── add / repair path ──────────────────────────────────────────

    #[tokio::test]
    async fn add_existing_empty_key_alias_takes_repair_path() {
        // Half-finished onboarding state: alias has the right id
        // but an empty key (mint-key failed last time). A retry of
        // `veda workspace add <alias>` must skip create_workspace
        // (no second server-side workspace) and just mint a key.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-stuck/keys$"))
            .respond_with(ok(json!({ "key": "wk-repaired" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        cfg.set_active_profile(
            "scratch",
            WorkspaceEntry {
                id: Some("ws-stuck".into()),
                key: String::new(),
            },
        );
        let out = run_workspace_add(&client, &mut cfg, "scratch".into(), None)
            .await
            .unwrap();
        assert!(out.repaired);
        assert_eq!(out.workspace_id, "ws-stuck");
        // Repair on the lone profile must not re-announce auto-switch.
        assert!(!out.auto_switched);
        assert_eq!(cfg.workspace_for("scratch").unwrap().key, "wk-repaired");
    }

    #[tokio::test]
    async fn add_repair_rejects_disagreeing_workspace_id() {
        // If the alias already has a stored id and the user passes
        // --workspace-id <different>, that's almost always a typo /
        // misunderstanding — silently honouring the new id would
        // abandon the server-side workspace from the original
        // half-finished onboarding. Reject with a clear pointer.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        cfg.set_active_profile(
            "scratch",
            WorkspaceEntry {
                id: Some("ws-original".into()),
                key: String::new(),
            },
        );
        let err = run_workspace_add(
            &client,
            &mut cfg,
            "scratch".into(),
            Some("ws-different".into()),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ws-original"), "msg: {msg}");
        assert!(msg.contains("ws-different"), "msg: {msg}");
        assert!(msg.contains("workspace rm"), "msg: {msg}");
    }

    #[tokio::test]
    async fn add_mint_failure_leaves_id_placeholder_for_repair() {
        // Path 1 + mint failure: create_workspace succeeds, the id
        // is persisted as an empty-key placeholder, then the
        // mint-key call fails. The function returns Err, but the
        // caller persists the partial-state cfg so the next
        // `veda workspace add scratch` hits the repair branch
        // instead of creating a second server-side workspace.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!({ "id": "ws-orphan-bait" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-orphan-bait/keys$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("milvus down"))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        let err = run_workspace_add(&client, &mut cfg, "scratch".into(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mint workspace key failed"));
        // Placeholder must be on cfg in memory so the caller
        // (main.rs) can flush it to disk.
        let entry = cfg.workspace_for("scratch").unwrap();
        assert_eq!(entry.id.as_deref(), Some("ws-orphan-bait"));
        assert!(entry.key.is_empty(), "key should be empty placeholder");
        // First-profile auto-switch fires on the placeholder too —
        // otherwise the user couldn't run `veda status` after the
        // failure and see the broken state at all.
        assert_eq!(cfg.active_alias(), Some("scratch"));
    }

    #[tokio::test]
    async fn add_duplicate_alias_is_rejected_before_http() {
        // Pre-existing alias must be rejected up front — otherwise
        // we'd burn a server-side workspace + key just to discard
        // them, plus risk silently overwriting an in-use profile.
        let server = MockServer::start().await;
        // expect(0) on a wildcard: any HTTP call fails the test.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        cfg.workspaces.insert(
            "scratch".into(),
            WorkspaceEntry {
                id: Some("old-id".into()),
                key: "old-key".into(),
            },
        );
        let err = run_workspace_add(&client, &mut cfg, "scratch".into(), None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("scratch"), "msg: {msg}");
        assert!(msg.contains("already exists"), "msg: {msg}");
        assert_eq!(cfg.workspace_for("scratch").unwrap().key, "old-key");
    }

    #[tokio::test]
    async fn add_second_profile_does_not_auto_switch() {
        // Auto-switch only kicks in when there's no other profile.
        // Adding a second one keeps the active alias on the original
        // so users don't get surprise context changes.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/workspaces"))
            .respond_with(ok(json!({ "id": "ws-second" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/workspaces/ws-second/keys$"))
            .respond_with(ok(json!({ "key": "wk-second" })))
            .mount(&server)
            .await;
        let client = Client::new(&server.uri());
        let mut cfg = cfg_with_api_key();
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-first".into()),
                key: "wk-first".into(),
            },
        );
        let out = run_workspace_add(&client, &mut cfg, "work".into(), None)
            .await
            .unwrap();
        assert!(!out.auto_switched);
        assert_eq!(cfg.active_alias(), Some("default"));
    }

    // ── switch ─────────────────────────────────────────────────────

    #[test]
    fn switch_returns_prev_alias_and_updates_active() {
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-1".into()),
                key: "wk-1".into(),
            },
        );
        cfg.workspaces.insert(
            "work".into(),
            WorkspaceEntry {
                id: Some("ws-2".into()),
                key: "wk-2".into(),
            },
        );
        let prev = run_workspace_switch(&mut cfg, "work".into()).unwrap();
        assert_eq!(prev, "default");
        assert_eq!(cfg.active_alias(), Some("work"));
    }

    #[test]
    fn switch_unknown_alias_rejected() {
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-1".into()),
                key: "wk-1".into(),
            },
        );
        let err = run_workspace_switch(&mut cfg, "ghost".into()).unwrap_err();
        // Original active must be unchanged.
        assert_eq!(cfg.active_alias(), Some("default"));
        assert!(err.to_string().contains("ghost"), "msg: {err}");
    }

    // ── rm ─────────────────────────────────────────────────────────

    #[test]
    fn rm_active_alias_rejected() {
        // Removing the active profile would leave the CLI in a
        // half-configured state — friendlier to nudge users to
        // switch first.
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-1".into()),
                key: "wk-1".into(),
            },
        );
        let err = run_workspace_rm(&mut cfg, "default").unwrap_err();
        assert!(err.to_string().contains("active"), "msg: {err}");
        assert!(cfg.workspaces.contains_key("default"));
    }

    #[test]
    fn rm_nonexistent_alias_rejected() {
        let mut cfg = CliConfig::default();
        let err = run_workspace_rm(&mut cfg, "ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"), "msg: {err}");
    }

    #[test]
    fn rm_non_active_profile_succeeds_alias_only() {
        // Alias-only delete: the entry vanishes from cfg.workspaces
        // but the function never makes an HTTP call (the server-side
        // wk_ stays valid, documented behaviour).
        let mut cfg = CliConfig::default();
        cfg.set_active_profile(
            "default",
            WorkspaceEntry {
                id: Some("ws-1".into()),
                key: "wk-1".into(),
            },
        );
        cfg.workspaces.insert(
            "work".into(),
            WorkspaceEntry {
                id: Some("ws-2".into()),
                key: "wk-2".into(),
            },
        );
        run_workspace_rm(&mut cfg, "work").unwrap();
        assert!(!cfg.workspaces.contains_key("work"));
        // active alias untouched.
        assert_eq!(cfg.active_alias(), Some("default"));
    }
}

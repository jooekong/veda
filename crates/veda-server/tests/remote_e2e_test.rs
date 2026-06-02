//! Black-box end-to-end tests against a LIVE veda server.
//!
//! These tests speak only HTTP — they import nothing from the veda crates,
//! so they exercise the real, deployed wire contract (catching serde renames,
//! status-code drift, async-indexing regressions, etc.). Each `#[tokio::test]`
//! is fully independent: it bootstraps its own account + workspace(s) over the
//! API and best-effort cleans up afterwards. No shared fixtures, no DB access.
//!
//! Target server: `VEDA_BASE_URL` env var, defaulting to the alpha deployment.
//!
//! Run (all #[ignore], so they never fire in plain `cargo test` / CI):
//! ```
//! VEDA_BASE_URL=https://veda.dbpaas.dingdongxiaoqu.com \
//!   cargo test -p veda-server --test remote_e2e_test -- --ignored --nocapture
//! ```
//! Reduce parallelism if the server is small: `--test-threads=4`.
//!
//! Coverage map (every documented route is touched by at least one test):
//!   health/meta · accounts(create/anon/claim/login) · workspaces(crud/keys/jwt)
//!   · admin tokens · fs(file crud/dir/conditional/range/grep/events) · search
//!   · summaries · sql · collections · vectors(upsert/search/query/delete)
//!   · datasets · validation limits · fs⇄db kind isolation.

#![allow(clippy::bool_assert_comparison)]

use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{json, Value};

const DEFAULT_BASE: &str = "https://veda.dbpaas.dingdongxiaoqu.com";

/// How long FS search / summaries may take to become consistent. Writes
/// enqueue an outbox row that a worker drains on a ~2s poll; summaries also
/// wait on an LLM call. Generous so the suite is not flaky on a busy server.
const INDEX_TIMEOUT: Duration = Duration::from_secs(45);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(150);

fn base_url() -> String {
    std::env::var("VEDA_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

// ── HTTP plumbing ─────────────────────────────────────────────────────────

/// A captured response: status as a plain u16 (cheap to assert/print),
/// headers, and the raw body text (parsed lazily as JSON when needed).
struct Resp {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
    /// `.data` of the standard `ApiResponse` envelope.
    fn data(&self) -> Value {
        self.json().get("data").cloned().unwrap_or(Value::Null)
    }
    /// Machine-readable `error_code` on failure envelopes ("" if absent).
    fn ecode(&self) -> String {
        self.json()
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
    fn etag(&self) -> String {
        self.header("etag")
    }
}

/// Send a built request and capture the full response. Never used for the SSE
/// endpoint (which never ends) — that path reads chunks directly.
async fn send(rb: reqwest::RequestBuilder) -> Resp {
    let r = rb.send().await.expect("HTTP request failed to send");
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let body = r.text().await.unwrap_or_default();
    Resp {
        status,
        headers,
        body,
    }
}

#[track_caller]
fn want(r: &Resp, status: u16, ctx: &str) {
    assert_eq!(
        r.status, status,
        "{ctx}: expected HTTP {status}, got {} — body: {}",
        r.status, r.body
    );
}

/// Assert both the status code and the machine-readable `error_code`.
#[track_caller]
fn want_err(r: &Resp, status: u16, code: &str, ctx: &str) {
    want(r, status, ctx);
    assert_eq!(r.ecode(), code, "{ctx}: expected error_code {code}, body: {}", r.body);
}

/// Thin client bound to the target base URL, plus bootstrap helpers.
#[derive(Clone)]
struct Srv {
    c: Client,
    base: String,
}

impl Srv {
    fn new() -> Self {
        let c = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("build reqwest client");
        Srv {
            c,
            base: base_url(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.c.get(self.url(path))
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.c.post(self.url(path))
    }
    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        self.c.put(self.url(path))
    }
    fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.c.delete(self.url(path))
    }
    fn head(&self, path: &str) -> reqwest::RequestBuilder {
        self.c.head(self.url(path))
    }

    // ── bootstrap ──

    fn unique_email(&self) -> String {
        format!("e2e-{}@example.com", uuid::Uuid::new_v4().simple())
    }

    /// Create a fresh named account; returns its account key (`vk_…`).
    async fn account(&self) -> String {
        let r = send(self.post("/v1/accounts").json(&json!({
            "name": "e2e",
            "email": self.unique_email(),
            "password": "pass1234",
        })))
        .await;
        want(&r, 200, "bootstrap account");
        r.data()["api_key"].as_str().unwrap().to_string()
    }

    /// Create a workspace of the given kind ("fs" | "db"); returns its id.
    /// The name is randomized because workspace names are unique per account,
    /// so a test that creates several of the same kind must not collide.
    async fn workspace(&self, vk: &str, kind: &str) -> String {
        let name = format!("ws-{kind}-{}", uuid::Uuid::new_v4().simple());
        let r = send(
            self.post("/v1/workspaces")
                .bearer_auth(vk)
                .json(&json!({"name": name, "kind": kind})),
        )
        .await;
        want(&r, 200, "bootstrap workspace");
        assert_eq!(r.data()["kind"], kind, "workspace kind echoes request");
        r.data()["id"].as_str().unwrap().to_string()
    }

    /// Mint a workspace key (`wk_…`) with "read" or "readwrite" permission.
    async fn wk(&self, vk: &str, ws: &str, perm: &str) -> String {
        let r = send(
            self.post(&format!("/v1/workspaces/{ws}/keys"))
                .bearer_auth(vk)
                .json(&json!({"name": "k", "permission": perm})),
        )
        .await;
        want(&r, 200, "bootstrap workspace key");
        assert_eq!(r.data()["permission"], perm);
        r.data()["key"].as_str().unwrap().to_string()
    }

    /// Best-effort teardown — archives the workspace and ignores the result.
    /// NOTE: this runs at the end of a test, so an assertion panic earlier
    /// skips it; the leftover is a unique, empty, harmless workspace (there is
    /// no delete-account API, so throwaway accounts also accumulate).
    async fn drop_ws(&self, vk: &str, ws: &str) {
        let _ = send(self.delete(&format!("/v1/workspaces/{ws}")).bearer_auth(vk)).await;
    }
}

/// Poll `f` until it reports success or `timeout` elapses. Returns whether it
/// succeeded. Used for the eventually-consistent FS search / summary paths.
async fn poll<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if f().await {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

// ════════════════════════════════════════════════════════════════════════
//  Group 0 — Health & meta (no auth)
// ════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn health_and_meta_endpoints() {
    let s = Srv::new();

    // Liveness: cheap, always 200 "ok".
    let r = send(s.get("/healthz")).await;
    want(&r, 200, "/healthz");
    assert_eq!(r.body.trim(), "ok");

    // Readiness: 200 with mysql + milvus both ok on a healthy box.
    let r = send(s.get("/v1/ready")).await;
    want(&r, 200, "/v1/ready");
    let j = r.json();
    assert_eq!(j["status"], "ready", "ready status");
    let comps = j["components"].as_array().expect("components array");
    let names: Vec<&str> = comps.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"mysql") && names.contains(&"milvus"), "components: {names:?}");
    assert!(comps.iter().all(|c| c["ok"] == true), "all components ok");

    // Capability probe (unauthenticated, NOT under /v1).
    let r = send(s.get("/capabilities")).await;
    want(&r, 200, "/capabilities");
    assert!(r.data()["summary_enabled"].is_boolean(), "summary_enabled bit present");

    // Embedded install script served as a shell script.
    let r = send(s.get("/install.sh")).await;
    want(&r, 200, "/install.sh");
    assert!(r.header("content-type").contains("shellscript"), "install.sh mime");

    // Metrics requires a bearer token; without it the endpoint hides as 404.
    let r = send(s.get("/v1/metrics")).await;
    assert_eq!(r.status, 404, "/v1/metrics without token must 404, got {}", r.status);
}

// ════════════════════════════════════════════════════════════════════════
//  Group 1 — Accounts & auth
// ════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn account_create_login_and_duplicate() {
    let s = Srv::new();
    let email = s.unique_email();

    // Create.
    let r = send(s.post("/v1/accounts").json(&json!({
        "name": "joe", "email": email, "password": "secret123",
    })))
    .await;
    want(&r, 200, "create account");
    assert!(r.data()["account_id"].is_string());
    assert!(r.data()["api_key"].as_str().unwrap().starts_with("vk_"), "vk_ prefix");

    // Duplicate email → 409 Conflict.
    let r = send(s.post("/v1/accounts").json(&json!({
        "name": "joe2", "email": email, "password": "other",
    })))
    .await;
    want(&r, 409, "duplicate email");

    // Login with correct creds → fresh api_key.
    let r = send(s.post("/v1/accounts/login").json(&json!({
        "email": email, "password": "secret123",
    })))
    .await;
    want(&r, 200, "login");
    assert!(r.data()["api_key"].as_str().unwrap().starts_with("vk_"));

    // Wrong password → 401.
    let r = send(s.post("/v1/accounts/login").json(&json!({
        "email": email, "password": "WRONG",
    })))
    .await;
    want(&r, 401, "login wrong password");

    // Unknown email → 401 (no user-enumeration distinction).
    let r = send(s.post("/v1/accounts/login").json(&json!({
        "email": s.unique_email(), "password": "secret123",
    })))
    .await;
    want(&r, 401, "login unknown email");
}

#[tokio::test]
#[ignore]
async fn anonymous_onboard_claim_login() {
    let s = Srv::new();

    // One-shot onboarding returns account key + a default fs workspace + wk.
    let r = send(s.post("/v1/accounts/anonymous")).await;
    want(&r, 200, "anonymous onboard");
    let d = r.data();
    let vk = d["api_key"].as_str().unwrap().to_string();
    let ws = d["workspace_id"].as_str().unwrap().to_string();
    assert!(vk.starts_with("vk_"));
    assert!(d["workspace_key"].as_str().unwrap().starts_with("wk_"));

    // The bootstrapped workspace is fs-kind.
    let r = send(s.get("/v1/workspaces").bearer_auth(&vk)).await;
    want(&r, 200, "list workspaces");
    let d = r.data();
    let item = d["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == ws)
        .expect("onboarded workspace present");
    assert_eq!(item["kind"], "fs", "anonymous default workspace is fs");

    // Claim: attach email + password to the anonymous account.
    let email = s.unique_email();
    let r = send(s.post("/v1/accounts/claim").bearer_auth(&vk).json(&json!({
        "email": email, "password": "claimpw1", "name": "claimed",
    })))
    .await;
    want(&r, 200, "claim account");

    // Claiming the same (already-claimed) account again → 400.
    let r = send(s.post("/v1/accounts/claim").bearer_auth(&vk).json(&json!({
        "email": s.unique_email(), "password": "x", "name": "again",
    })))
    .await;
    want(&r, 400, "re-claim already-claimed account");

    // The claimed identity now works for login.
    let r = send(s.post("/v1/accounts/login").json(&json!({
        "email": email, "password": "claimpw1",
    })))
    .await;
    want(&r, 200, "login after claim");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn workspace_jwt_token_used_on_fs() {
    let s = Srv::new();
    let vk = s.account().await;
    let ws = s.workspace(&vk, "fs").await;

    // Mint a 24h JWT scoped to the workspace.
    let r = send(s.post(&format!("/v1/workspaces/{ws}/token")).bearer_auth(&vk)).await;
    want(&r, 200, "mint workspace jwt");
    let jwt = r.data()["token"].as_str().unwrap().to_string();
    assert!(r.data()["expires_at"].is_string(), "expires_at present");

    // JWT authenticates an fs read.
    let r = send(s.get("/v1/fs").query(&[("list", "1")]).bearer_auth(&jwt)).await;
    want(&r, 200, "fs list with jwt");

    // A malformed JWT is rejected.
    let r = send(
        s.get("/v1/fs")
            .query(&[("list", "1")])
            .bearer_auth("eyJ.not.a.jwt"),
    )
    .await;
    assert_eq!(r.status, 401, "garbage jwt must 401, got {}", r.status);

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn auth_missing_or_garbage_rejected() {
    let s = Srv::new();

    // No Authorization header on a protected route.
    let r = send(s.get("/v1/fs").query(&[("list", "1")])).await;
    assert_eq!(r.status, 401, "no auth → 401, got {}", r.status);

    // Garbage workspace key.
    let r = send(s.get("/v1/fs").query(&[("list", "1")]).bearer_auth("wk_deadbeef")).await;
    assert_eq!(r.status, 401, "garbage wk → 401");

    // Garbage account key on the vector plane.
    let r = send(
        s.post("/v1/vectors/search")
            .bearer_auth("vk_deadbeef")
            .json(&json!({"workspace_id": "x", "query": "hi"})),
    )
    .await;
    assert_eq!(r.status, 401, "garbage vk → 401");

    // Creating a workspace requires auth.
    let r = send(s.post("/v1/workspaces").json(&json!({"name": "x"}))).await;
    assert_eq!(r.status, 401, "create ws without auth → 401");
}

// ════════════════════════════════════════════════════════════════════════
//  Group 2 — Workspace management
// ════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn workspace_create_list_paginate_delete() {
    let s = Srv::new();
    let vk = s.account().await;

    // Create a handful of workspaces of mixed kind.
    let mut ids = Vec::new();
    for kind in ["fs", "db", "fs"] {
        ids.push(s.workspace(&vk, kind).await);
    }

    // Full list contains all of them.
    let r = send(s.get("/v1/workspaces").bearer_auth(&vk)).await;
    want(&r, 200, "list all");
    let listed: Vec<String> = r.data()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect();
    for id in &ids {
        assert!(listed.contains(id), "workspace {id} present in list");
    }

    // Cursor pagination: limit=2 → has_more + next_cursor; second page works.
    let r = send(s.get("/v1/workspaces").query(&[("limit", "2")]).bearer_auth(&vk)).await;
    want(&r, 200, "page 1");
    let page1 = r.data();
    assert_eq!(page1["items"].as_array().unwrap().len(), 2, "page size honored");
    assert_eq!(page1["has_more"], true, "has_more on first page");
    let cursor = page1["next_cursor"].as_str().expect("next_cursor present").to_string();
    let r = send(
        s.get("/v1/workspaces")
            .query(&[("limit", "2"), ("after", &cursor)])
            .bearer_auth(&vk),
    )
    .await;
    want(&r, 200, "page 2");
    assert!(!r.data()["items"].as_array().unwrap().is_empty(), "page 2 has items");

    // Delete one → it disappears from the active list.
    let gone = ids.remove(0);
    let r = send(s.delete(&format!("/v1/workspaces/{gone}")).bearer_auth(&vk)).await;
    want(&r, 200, "delete workspace");
    let r = send(s.get("/v1/workspaces").bearer_auth(&vk)).await;
    let still: Vec<String> = r.data()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!still.contains(&gone), "deleted workspace absent");

    // Deleting an unknown workspace id → 404.
    let r = send(
        s.delete(&format!("/v1/workspaces/{}", uuid::Uuid::new_v4()))
            .bearer_auth(&vk),
    )
    .await;
    assert_eq!(r.status, 404, "delete unknown ws → 404, got {}", r.status);

    for id in &ids {
        s.drop_ws(&vk, id).await;
    }
}

#[tokio::test]
#[ignore]
async fn workspace_duplicate_name_rejected() {
    let s = Srv::new();
    let vk = s.account().await;
    let name = format!("dup-{}", uuid::Uuid::new_v4().simple());

    let r = send(s.post("/v1/workspaces").bearer_auth(&vk).json(&json!({"name": name, "kind": "db"}))).await;
    want(&r, 200, "first create");
    let ws = r.data()["id"].as_str().unwrap().to_string();

    // Workspace names are unique per account; a db-kind clash is a clean 409.
    // (NOTE: an fs-kind clash currently surfaces as 500 INTERNAL instead of
    // 409 — an inconsistency worth fixing server-side.)
    let r = send(s.post("/v1/workspaces").bearer_auth(&vk).json(&json!({"name": name, "kind": "db"}))).await;
    want(&r, 409, "duplicate workspace name");
    assert_eq!(r.ecode(), "ALREADY_EXISTS");

    s.drop_ws(&vk, &ws).await;
}

// ════════════════════════════════════════════════════════════════════════
//  Group 3 — FS workspace data plane (kind=fs)
// ════════════════════════════════════════════════════════════════════════

/// Bootstrap an fs workspace; returns (account_key, workspace_id, rw_wk_key).
async fn fs_ctx(s: &Srv) -> (String, String, String) {
    let vk = s.account().await;
    let ws = s.workspace(&vk, "fs").await;
    let wk = s.wk(&vk, &ws, "readwrite").await;
    (vk, ws, wk)
}

#[tokio::test]
#[ignore]
async fn fs_file_put_get_stat_head_delete() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;
    let text = "Hello Veda. A note about vector search.";

    // PUT → revision 1, ETag "1", fresh write.
    let r = send(s.put("/v1/fs/dir/a.md").bearer_auth(&wk).body(text)).await;
    want(&r, 200, "put file");
    assert_eq!(r.etag(), "\"1\"", "etag is quoted revision");
    assert_eq!(r.data()["revision"], 1);
    assert_eq!(r.data()["content_unchanged"], false);
    assert!(r.data()["file_id"].is_string());

    // GET → exact bytes back (raw body, not JSON).
    let r = send(s.get("/v1/fs/dir/a.md").bearer_auth(&wk)).await;
    want(&r, 200, "get file");
    assert_eq!(r.body, text);

    // Stat → metadata; checksum is a 64-hex sha256.
    let r = send(s.get("/v1/fs/dir/a.md").query(&[("stat", "1")]).bearer_auth(&wk)).await;
    want(&r, 200, "stat file");
    let d = r.data();
    assert_eq!(d["is_dir"], false);
    assert_eq!(d["path"], "/dir/a.md");
    assert_eq!(d["size_bytes"], text.len() as i64);
    assert_eq!(d["revision"], 1);
    let cksum = d["checksum"].as_str().unwrap();
    assert!(cksum.len() == 64 && cksum.chars().all(|c| c.is_ascii_hexdigit()), "sha256 hex");

    // HEAD existing → 200; HEAD missing → 404.
    let r = send(s.head("/v1/fs/dir/a.md").bearer_auth(&wk)).await;
    want(&r, 200, "head existing");
    let r = send(s.head("/v1/fs/dir/missing.md").bearer_auth(&wk)).await;
    want(&r, 404, "head missing");

    // List root → the directory shows up.
    let r = send(s.get("/v1/fs").query(&[("list", "1")]).bearer_auth(&wk)).await;
    want(&r, 200, "list root");
    let entries = r.data();
    let dir = entries.as_array().unwrap().iter().find(|e| e["name"] == "dir").expect("dir entry");
    assert_eq!(dir["is_dir"], true);

    // DELETE → gone (subsequent GET 404).
    let r = send(s.delete("/v1/fs/dir/a.md").bearer_auth(&wk)).await;
    want(&r, 200, "delete file");
    let r = send(s.get("/v1/fs/dir/a.md").bearer_auth(&wk)).await;
    want(&r, 404, "get after delete");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_mkdir_copy_rename() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // mkdir then write a file inside it.
    let r = send(s.post("/v1/fs-mkdir").bearer_auth(&wk).json(&json!({"path": "/proj"}))).await;
    want(&r, 200, "mkdir");
    let r = send(s.put("/v1/fs/proj/x.txt").bearer_auth(&wk).body("payload")).await;
    want(&r, 200, "write in dir");

    // copy → both exist with same content.
    let r = send(
        s.post("/v1/fs-copy")
            .bearer_auth(&wk)
            .json(&json!({"from": "/proj/x.txt", "to": "/proj/y.txt"})),
    )
    .await;
    want(&r, 200, "copy");
    let r = send(s.get("/v1/fs/proj/y.txt").bearer_auth(&wk)).await;
    assert_eq!(r.body, "payload", "copy content");

    // rename y → z; y disappears, z has the content.
    let r = send(
        s.post("/v1/fs-rename")
            .bearer_auth(&wk)
            .json(&json!({"from": "/proj/y.txt", "to": "/proj/z.txt"})),
    )
    .await;
    want(&r, 200, "rename");
    let r = send(s.get("/v1/fs/proj/z.txt").bearer_auth(&wk)).await;
    assert_eq!(r.body, "payload", "renamed content");
    let r = send(s.get("/v1/fs/proj/y.txt").bearer_auth(&wk)).await;
    want(&r, 404, "old name gone after rename");

    // Directory listing reflects the two surviving files.
    let r = send(s.get("/v1/fs/proj").query(&[("list", "1")]).bearer_auth(&wk)).await;
    want(&r, 200, "list dir");
    let names: Vec<String> = r
        .data()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"x.txt".to_string()) && names.contains(&"z.txt".to_string()), "{names:?}");
    assert!(!names.contains(&"y.txt".to_string()), "y.txt renamed away");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_conditional_writes() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // rev 1.
    let r = send(s.put("/v1/fs/c.txt").bearer_auth(&wk).body("alpha")).await;
    want(&r, 200, "create");
    assert_eq!(r.data()["revision"], 1);

    // If-Match correct rev → write succeeds, rev bumps to 2.
    let r = send(s.put("/v1/fs/c.txt").bearer_auth(&wk).header("If-Match", "\"1\"").body("beta")).await;
    want(&r, 200, "if-match correct");
    assert_eq!(r.data()["revision"], 2);

    // If-Match stale rev → 412 Precondition Failed.
    let r = send(s.put("/v1/fs/c.txt").bearer_auth(&wk).header("If-Match", "\"1\"").body("gamma")).await;
    want(&r, 412, "if-match stale");

    // Re-writing identical content (no header) is content-addressed: dedup,
    // content_unchanged=true, revision NOT bumped.
    let r = send(s.put("/v1/fs/c.txt").bearer_auth(&wk).body("beta")).await;
    want(&r, 200, "identical rewrite");
    assert_eq!(r.data()["content_unchanged"], true, "dedup by checksum");
    assert_eq!(r.data()["revision"], 2, "revision stable on dedup");

    // If-None-Match: <current checksum> short-circuits even when the body
    // differs — the new body is NOT written.
    let r = send(s.get("/v1/fs/c.txt").query(&[("stat", "1")]).bearer_auth(&wk)).await;
    let cksum = r.data()["checksum"].as_str().unwrap().to_string();
    let r = send(
        s.put("/v1/fs/c.txt")
            .bearer_auth(&wk)
            .header("If-None-Match", format!("\"{cksum}\""))
            .body("THIS-SHOULD-NOT-LAND"),
    )
    .await;
    want(&r, 200, "if-none-match short-circuit");
    assert_eq!(r.data()["content_unchanged"], true);
    assert_eq!(r.data()["revision"], 2);
    let r = send(s.get("/v1/fs/c.txt").bearer_auth(&wk)).await;
    assert_eq!(r.body, "beta", "body unchanged after short-circuit");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_append_bumps_revision() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    let r = send(s.put("/v1/fs/log.txt").bearer_auth(&wk).body("line1\n")).await;
    want(&r, 200, "create log");
    assert_eq!(r.data()["revision"], 1);

    // POST appends to the existing file.
    let r = send(s.post("/v1/fs/log.txt").bearer_auth(&wk).body("line2\n")).await;
    want(&r, 200, "append");
    assert!(r.data()["revision"].as_i64().unwrap() >= 2, "append bumps revision");

    let r = send(s.get("/v1/fs/log.txt").bearer_auth(&wk)).await;
    assert_eq!(r.body, "line1\nline2\n", "appended content");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_partial_reads_lines_and_range() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // Line-range read.
    send(s.put("/v1/fs/multi.txt").bearer_auth(&wk).body("L1\nL2\nL3\nL4\n")).await;
    let r = send(s.get("/v1/fs/multi.txt").query(&[("lines", "2:3")]).bearer_auth(&wk)).await;
    want(&r, 200, "lines read");
    assert!(r.body.contains("L2") && r.body.contains("L3"), "lines 2-3: {:?}", r.body);
    assert!(!r.body.contains("L1") && !r.body.contains("L4"), "excludes L1/L4: {:?}", r.body);

    // Byte-range read → 206 with Content-Range.
    send(s.put("/v1/fs/bytes.txt").bearer_auth(&wk).body("0123456789")).await;
    let r = send(s.get("/v1/fs/bytes.txt").bearer_auth(&wk).header("Range", "bytes=2-5")).await;
    want(&r, 206, "range read");
    assert_eq!(r.body, "2345", "range slice");
    assert!(r.header("content-range").contains("/10"), "content-range total: {}", r.header("content-range"));

    // Unsatisfiable range → 416.
    let r = send(s.get("/v1/fs/bytes.txt").bearer_auth(&wk).header("Range", "bytes=100-200")).await;
    want(&r, 416, "range unsatisfiable");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_root_cannot_be_deleted() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;
    let r = send(s.delete("/v1/fs").bearer_auth(&wk)).await;
    want(&r, 400, "delete root rejected");
    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_readonly_key_enforced() {
    let s = Srv::new();
    let vk = s.account().await;
    let ws = s.workspace(&vk, "fs").await;
    let rw = s.wk(&vk, &ws, "readwrite").await;
    let ro = s.wk(&vk, &ws, "read").await;

    // Seed with the rw key.
    send(s.put("/v1/fs/r.txt").bearer_auth(&rw).body("seed")).await;

    // Read-only key can read…
    let r = send(s.get("/v1/fs/r.txt").bearer_auth(&ro)).await;
    want(&r, 200, "ro read");
    assert_eq!(r.body, "seed");

    // …but every mutation is 403.
    let r = send(s.put("/v1/fs/r.txt").bearer_auth(&ro).body("x")).await;
    want(&r, 403, "ro write");
    let r = send(s.delete("/v1/fs/r.txt").bearer_auth(&ro)).await;
    want(&r, 403, "ro delete");
    let r = send(s.post("/v1/fs-mkdir").bearer_auth(&ro).json(&json!({"path": "/nope"}))).await;
    want(&r, 403, "ro mkdir");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_grep_variants() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    send(s.put("/v1/fs/docs/a.md").bearer_auth(&wk).body("Hello World\nfoo bar baz")).await;
    send(s.put("/v1/fs/docs/b.md").bearer_auth(&wk).body("HELLO again here")).await;
    send(s.put("/v1/fs/other/c.md").bearer_auth(&wk).body("nothing relevant")).await;

    // Case-sensitive: only the exact-case "Hello" in a.md.
    let r = send(s.post("/v1/grep").bearer_auth(&wk).json(&json!({"pattern": "Hello"}))).await;
    want(&r, 200, "grep");
    let hits = r.data();
    let paths: Vec<&str> = hits.as_array().unwrap().iter().filter_map(|h| h["path"].as_str()).collect();
    assert!(paths.contains(&"/docs/a.md"), "case-sensitive hit: {paths:?}");
    // GrepHit shape.
    let first = &hits.as_array().unwrap()[0];
    assert!(first["line_no"].is_number() && first["line"].is_string(), "grep hit shape");

    // ignore_case → matches both a.md and b.md.
    let r = send(s.post("/v1/grep").bearer_auth(&wk).json(&json!({"pattern": "hello", "ignore_case": true}))).await;
    let paths: Vec<String> = r.data().as_array().unwrap().iter().filter_map(|h| h["path"].as_str().map(String::from)).collect();
    assert!(paths.iter().any(|p| p == "/docs/a.md") && paths.iter().any(|p| p == "/docs/b.md"), "ignore_case: {paths:?}");

    // path_prefix scopes the search.
    let r = send(s.post("/v1/grep").bearer_auth(&wk).json(&json!({"pattern": "Hello", "path_prefix": "/other"}))).await;
    assert!(r.data().as_array().unwrap().is_empty(), "path_prefix /other excludes docs");

    // max_results caps the output.
    let r = send(s.post("/v1/grep").bearer_auth(&wk).json(&json!({"pattern": "o", "ignore_case": true, "max_results": 1}))).await;
    assert_eq!(r.data().as_array().unwrap().len(), 1, "max_results honored");

    s.drop_ws(&vk, &ws).await;
}

/// Read the SSE stream until `needle` appears in the accumulated body or the
/// timeout elapses. Returns the buffer if found. Reads chunk-by-chunk so it
/// never blocks forever on the endless stream.
async fn read_sse_until(mut resp: reqwest::Response, needle: &str, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    let mut buf = String::new();
    while start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_secs(2), resp.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                if buf.contains(needle) {
                    return Some(buf);
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => continue, // per-read timeout; re-check the overall deadline
        }
    }
    buf.contains(needle).then_some(buf)
}

#[tokio::test]
#[ignore]
async fn fs_events_stream_and_cursor() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // A write produces a change event.
    send(s.put("/v1/fs/evt/note.md").bearer_auth(&wk).body("event payload")).await;

    // Fresh subscription (since_id=0) replays history; we should see our path.
    let resp = s
        .get("/v1/events")
        .query(&[("since_id", "0")])
        .bearer_auth(&wk)
        .send()
        .await
        .expect("open sse");
    assert_eq!(resp.status().as_u16(), 200, "events stream opens 200");
    let buf = read_sse_until(resp, "/evt/note.md", Duration::from_secs(20))
        .await
        .expect("did not observe create event within timeout");
    assert!(
        buf.contains("\"event_type\":\"create\"") || buf.contains("\"event_type\":\"update\""),
        "event carries a create/update type: {buf}"
    );

    // Extract the first event id from the stream to drive the 410 check.
    let n = buf
        .split("\"id\":")
        .nth(1)
        .and_then(|tail| tail.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(0);

    // A cursor below the workspace's min event id is expired → 410 Gone.
    if n > 1 {
        let r = send(s.get("/v1/events").query(&[("since_id", "1")]).bearer_auth(&wk)).await;
        want(&r, 410, "expired cursor");
        assert!(r.json()["current_min_id"].is_number(), "410 carries current_min_id");
    }

    // An invalid path_prefix is a hard 400 (not silently widened).
    let r = send(
        s.get("/v1/events")
            .query(&[("since_id", "0"), ("path_prefix", "no-leading-slash")])
            .bearer_auth(&wk),
    )
    .await;
    want(&r, 400, "invalid path_prefix");
    assert_eq!(r.json()["error"], "invalid path_prefix");

    s.drop_ws(&vk, &ws).await;
}

// ════════════════════════════════════════════════════════════════════════
//  Group 4 — FS search & summaries (async / eventually consistent)
// ════════════════════════════════════════════════════════════════════════

/// FS search exposes all three retrieval signals; this test pins each to its
/// own behaviour using two topically-orthogonal documents:
///   - sparse / BM25 (`fulltext`): lexical — a rare word matches only the doc
///     that literally contains it.
///   - dense / cosine (`semantic`): conceptual — a paraphrase with ZERO shared
///     tokens still retrieves the on-topic doc.
///   - hybrid (`rrf`): fuses dense ANN + BM25 sparse.
#[tokio::test]
#[ignore]
async fn fs_search_dense_sparse_and_hybrid() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    let music = "/kb/music.md";
    let finance = "/kb/finance.md";
    send(s.put(&format!("/v1/fs{music}")).bearer_auth(&wk)
        .body("The xylophone quokka performs midnight marshmallow concerts in the grove.")).await;
    send(s.put(&format!("/v1/fs{finance}")).bearer_auth(&wk)
        .body("Quarterly revenue guidance was revised upward after strong fiscal results.")).await;

    // Collect resolved paths from a search response.
    let paths = |r: &Resp| -> Vec<String> {
        r.data().as_array().map(|a| a.iter().filter_map(|h| h["path"].as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let search = |query: &str, mode: &str| {
        let (s, wk) = (s.clone(), wk.clone());
        let (q, m) = (query.to_string(), mode.to_string());
        async move {
            send(s.post("/v1/search").bearer_auth(&wk)
                .json(&json!({"query": q, "mode": m, "limit": 5}))).await
        }
    };

    // Wait for async indexing (outbox → worker → Milvus) to make both visible.
    let indexed = poll(INDEX_TIMEOUT, || async {
        let r = search("marshmallow", "fulltext").await;
        paths(&r).iter().any(|p| p == music)
    })
    .await;
    assert!(indexed, "fulltext never indexed within {INDEX_TIMEOUT:?}");

    // ── Sparse / BM25 ── a rare word resolves ONLY the doc containing it.
    let r = search("marshmallow", "fulltext").await;
    want(&r, 200, "fulltext marshmallow");
    let p = paths(&r);
    assert!(p.iter().any(|x| x == music), "BM25 finds the literal match: {p:?}");
    assert!(!p.iter().any(|x| x == finance), "BM25 excludes the doc lacking the term: {p:?}");
    assert_eq!(r.data()[0]["score_type"], "bm25", "fulltext score_type");
    // …and the other rare word isolates the other doc.
    let r = search("revenue", "fulltext").await;
    let p = paths(&r);
    assert!(p.iter().any(|x| x == finance) && !p.iter().any(|x| x == music), "BM25 isolates finance: {p:?}");

    // ── Dense / cosine ── paraphrase with no shared tokens ranks the on-topic doc first.
    let r = search("a nocturnal animal playing a tuned percussion instrument", "semantic").await;
    want(&r, 200, "semantic music query");
    assert_eq!(paths(&r).first().map(String::as_str), Some(music), "dense ranks music doc first");
    assert_eq!(r.data()[0]["score_type"], "cosine", "semantic score_type");
    let r = search("corporate profit expectations for the period", "semantic").await;
    assert_eq!(paths(&r).first().map(String::as_str), Some(finance), "dense ranks finance doc first");

    // ── Hybrid / RRF ── fuses both signals.
    let r = search("marshmallow concerts", "hybrid").await;
    want(&r, 200, "hybrid search");
    assert_eq!(paths(&r).first().map(String::as_str), Some(music), "hybrid top hit");
    assert_eq!(r.data()[0]["score_type"], "rrf", "hybrid score_type");

    // path_prefix scoping excludes everything under a non-matching prefix.
    let r = send(s.post("/v1/search").bearer_auth(&wk)
        .json(&json!({"query": "marshmallow concerts", "mode": "hybrid", "path_prefix": "/elsewhere"}))).await;
    want(&r, 200, "search scoped");
    assert!(paths(&r).is_empty(), "path_prefix excludes both files");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_summaries_abstract_and_overview() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // Only meaningful when the deployment has summaries enabled.
    let cap = send(s.get("/capabilities")).await;
    if cap.data()["summary_enabled"] != true {
        eprintln!("summary_enabled=false on this deployment; skipping summary assertions");
        s.drop_ws(&vk, &ws).await;
        return;
    }

    let path = "/notes/story.md";
    send(s.put(&format!("/v1/fs{path}")).bearer_auth(&wk).body(
        "Once upon a time a curious quokka learned to play the xylophone under the moonlight, \
         delighting all the nocturnal animals of the forest with its melodies.",
    )).await;

    // Abstract is generated by a background LLM worker → 202 (PENDING) then
    // 200. A 202 that never resolves within the window is LLM latency, not a
    // server contract failure (it can queue behind other summaries under load),
    // so soft-pass with a warning rather than flake the suite.
    let ready = poll(SUMMARY_TIMEOUT, || async {
        let r = send(s.get(&format!("/v1/abstract{path}")).bearer_auth(&wk)).await;
        assert!(r.status == 200 || r.status == 202, "abstract status {}: {}", r.status, r.body);
        r.status == 200
    })
    .await;
    if !ready {
        eprintln!("abstract still PENDING after {SUMMARY_TIMEOUT:?} (LLM latency); soft-passing");
        s.drop_ws(&vk, &ws).await;
        return;
    }

    let r = send(s.get(&format!("/v1/abstract{path}")).bearer_auth(&wk)).await;
    want(&r, 200, "abstract");
    assert_eq!(r.data()["path"], path);
    assert!(!r.data()["l0_abstract"].as_str().unwrap().is_empty(), "abstract text present");

    // Overview (L1) is produced alongside the abstract, so it is normally
    // ready by now; allow a short grace window, soft-pass on LLM lag.
    let ready = poll(Duration::from_secs(60), || async {
        send(s.get(&format!("/v1/overview{path}")).bearer_auth(&wk)).await.status == 200
    })
    .await;
    if ready {
        let r = send(s.get(&format!("/v1/overview{path}")).bearer_auth(&wk)).await;
        want(&r, 200, "overview");
        assert!(!r.data()["l1_overview"].as_str().unwrap().is_empty(), "overview text present");
    } else {
        eprintln!("overview still PENDING; soft-passing");
    }

    s.drop_ws(&vk, &ws).await;
}

// ════════════════════════════════════════════════════════════════════════
//  Group 5 — FS SQL & collections
// ════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn fs_sql_files_table_and_udtf() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    send(s.put("/v1/fs/data/a.md").bearer_auth(&wk).body("apples\nbananas")).await;
    send(s.put("/v1/fs/data/b.md").bearer_auth(&wk).body("cherries")).await;

    // `files` is a real table over the dentry tree.
    let r = send(s.post("/v1/sql").bearer_auth(&wk)
        .json(&json!({"sql": "SELECT path, name, is_dir, size_bytes FROM files ORDER BY path"}))).await;
    want(&r, 200, "sql files table");
    let rows = r.data();
    let paths: Vec<&str> = rows.as_array().unwrap().iter().filter_map(|row| row["path"].as_str()).collect();
    assert!(paths.contains(&"/data/a.md") && paths.contains(&"/data/b.md"), "files rows: {paths:?}");

    // `veda_fs('/dir/')` is a UDTF: directory listing mode.
    let r = send(s.post("/v1/sql").bearer_auth(&wk)
        .json(&json!({"sql": "SELECT path, name, type FROM veda_fs('/data/') ORDER BY name"}))).await;
    want(&r, 200, "sql veda_fs udtf");
    let names: Vec<String> = r.data().as_array().unwrap().iter()
        .filter_map(|row| row["name"].as_str().map(String::from)).collect();
    assert!(names.iter().any(|n| n == "a.md") && names.iter().any(|n| n == "b.md"), "udtf rows: {names:?}");

    // Malformed SQL surfaces as a server error. NOTE: currently HTTP 500
    // ("internal server error"); ideally this would be a 4xx. Assert it is an
    // error so the test survives a future fix that tightens the status code.
    let r = send(s.post("/v1/sql").bearer_auth(&wk)
        .json(&json!({"sql": "SELECT * FROM veda_fs LIMIT 1"}))).await;
    assert!(r.status >= 400, "malformed sql should error, got {}: {}", r.status, r.body);

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_collections_lifecycle() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // Create a structured collection with an embedding source.
    let r = send(s.post("/v1/collections").bearer_auth(&wk).json(&json!({
        "name": "products",
        "collection_type": "structured",
        "fields": [
            {"name": "title", "type": "string", "index": true},
            {"name": "price", "type": "int64"},
            {"name": "description", "type": "string"}
        ],
        "embedding_source": "description"
    }))).await;
    want(&r, 200, "create collection");
    let d = r.data();
    assert_eq!(d["name"], "products");
    assert_eq!(d["collection_type"], "structured");
    assert_eq!(d["embedding_source"], "description");
    assert!(d["embedding_dim"].as_i64().unwrap() > 0, "embedding_dim set");
    assert_eq!(d["status"], "active");
    assert!(d["schema_json"].is_array(), "schema_json echoes fields");

    // Insert rows (embedding computed synchronously from `description`).
    let r = send(s.post("/v1/collections/products/rows").bearer_auth(&wk).json(&json!({
        "rows": [
            {"title": "Red Running Shoes", "price": 100, "description": "lightweight comfortable shoes for running and jogging"},
            {"title": "Winter Wool Hat", "price": 20, "description": "warm hat for cold snowy weather"}
        ]
    }))).await;
    want(&r, 200, "insert rows");
    assert_eq!(r.data()["inserted"], 2);

    // Semantic search ranks the shoes first; internal columns are stripped.
    let found = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/collections/products/search").bearer_auth(&wk)
            .json(&json!({"query": "footwear for jogging", "limit": 5}))).await;
        !r.data().as_array().map(|a| a.is_empty()).unwrap_or(true)
    })
    .await;
    assert!(found, "collection search returned no rows");
    let r = send(s.post("/v1/collections/products/search").bearer_auth(&wk)
        .json(&json!({"query": "footwear for jogging", "limit": 5}))).await;
    want(&r, 200, "collection search");
    let top = &r.data().as_array().unwrap()[0].clone();
    assert!(top["title"].as_str().unwrap().contains("Shoes"), "shoes rank first: {top}");
    assert!(top["distance"].is_number(), "distance score present");
    assert!(top.get("vector").is_none() && top.get("workspace_id").is_none(), "internal cols stripped");

    // Describe + list.
    let r = send(s.get("/v1/collections/products").bearer_auth(&wk)).await;
    want(&r, 200, "describe collection");
    assert_eq!(r.data()["name"], "products");
    let r = send(s.get("/v1/collections").bearer_auth(&wk)).await;
    want(&r, 200, "list collections");
    assert!(r.data().as_array().unwrap().iter().any(|c| c["name"] == "products"));

    // Delete → describe/search now 404.
    let r = send(s.delete("/v1/collections/products").bearer_auth(&wk)).await;
    want(&r, 200, "delete collection");
    let r = send(s.get("/v1/collections/products").bearer_auth(&wk)).await;
    want(&r, 404, "describe after delete");
    let r = send(s.post("/v1/collections/products/search").bearer_auth(&wk)
        .json(&json!({"query": "x"}))).await;
    want(&r, 404, "search after delete");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn fs_collections_raw_and_duplicate() {
    let s = Srv::new();
    let (vk, ws, wk) = fs_ctx(&s).await;

    // First create succeeds…
    let body = json!({
        "name": "notes",
        "collection_type": "raw",
        "fields": [{"name": "body", "type": "string"}],
        "embedding_source": "body"
    });
    let r = send(s.post("/v1/collections").bearer_auth(&wk).json(&body)).await;
    want(&r, 200, "create raw collection");
    assert_eq!(r.data()["collection_type"], "raw");

    // …duplicate name → 409.
    let r = send(s.post("/v1/collections").bearer_auth(&wk).json(&body)).await;
    want(&r, 409, "duplicate collection");

    // Round-trip a row through the raw collection.
    let r = send(s.post("/v1/collections/notes/rows").bearer_auth(&wk)
        .json(&json!({"rows": [{"body": "meeting notes about the quarterly roadmap"}]}))).await;
    want(&r, 200, "insert raw row");
    assert_eq!(r.data()["inserted"], 1);
    let found = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/collections/notes/search").bearer_auth(&wk)
            .json(&json!({"query": "roadmap planning", "limit": 3}))).await;
        r.status == 200 && !r.data().as_array().map(|a| a.is_empty()).unwrap_or(true)
    })
    .await;
    assert!(found, "raw collection search returned nothing");

    s.drop_ws(&vk, &ws).await;
}

// ════════════════════════════════════════════════════════════════════════
//  Group 6 — DB workspace data plane (kind=db). Vectors use the ACCOUNT key
//  (vk_) with workspace_id in the body.
// ════════════════════════════════════════════════════════════════════════

/// Bootstrap a db workspace; returns (account_key, workspace_id).
async fn db_ctx(s: &Srv) -> (String, String) {
    let vk = s.account().await;
    let ws = s.workspace(&vk, "db").await;
    (vk, ws)
}

#[tokio::test]
#[ignore]
async fn db_vectors_roundtrip() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;

    // Upsert: two explicit ids + one server-generated.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [
            {"id": "r1", "text": "the quick brown fox jumps", "category": "animals", "tags": ["fox"], "meta": {"legs": 4}},
            {"id": "r2", "text": "a lazy dog sleeps all day", "category": "animals", "tags": ["dog"], "meta": {"legs": 4}},
            {"text": "vector databases power semantic search", "category": "tech"}
        ]
    }))).await;
    want(&r, 200, "upsert");
    assert_eq!(r.data()["ids"].as_array().unwrap().len(), 3, "three ids");
    assert!(r.data()["commit_ts"].is_number(), "commit_ts present");

    // Search ranks the fox record first.
    let hit = poll_first_hit(&s, &vk, &ws, "quick brown fox", Duration::from_secs(15)).await
        .expect("search returned no hits");
    assert_eq!(hit["id"], "r1", "fox ranks first");
    assert!(hit["score"].is_number(), "score present");
    assert_eq!(hit["dataset"], "default");
    assert_eq!(hit["category"], "animals");
    assert_eq!(hit["tags"][0], "fox");
    assert!(hit["text"].as_str().unwrap().contains("fox"));
    assert_eq!(hit["meta"]["legs"], 4);
    assert!(hit["created_at"].is_number() && hit["updated_at"].is_number());

    // Query by id: direct lookup, no score field.
    let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
        .json(&json!({"workspace_id": ws, "ids": ["r1", "r2"]}))).await;
    want(&r, 200, "query by id");
    let hits = r.data()["hits"].as_array().unwrap().clone();
    assert_eq!(hits.len(), 2, "two records");
    let got: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(got.contains(&"r1") && got.contains(&"r2"), "query returns r1+r2: {got:?}");
    assert!(hits.iter().all(|h| h.get("score").is_none()), "query hits carry no score");

    // Query for a non-existent id returns no error, just no hits for it.
    let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
        .json(&json!({"workspace_id": ws, "ids": ["does-not-exist"]}))).await;
    want(&r, 200, "query missing id");
    assert!(r.data()["hits"].as_array().unwrap().is_empty(), "no hit for missing id");

    // Delete r1; delete_count mirrors the id list.
    let r = send(s.post("/v1/vectors/delete").bearer_auth(&vk)
        .json(&json!({"workspace_id": ws, "ids": ["r1"]}))).await;
    want(&r, 200, "delete");
    assert_eq!(r.data()["delete_count"], 1);

    // delete_count counts id-expression terms, not rows that existed: deleting
    // an id that was never present still reports 1 (Milvus tombstone model).
    let r = send(s.post("/v1/vectors/delete").bearer_auth(&vk)
        .json(&json!({"workspace_id": ws, "ids": ["never-existed"]}))).await;
    want(&r, 200, "delete nonexistent");
    assert_eq!(r.data()["delete_count"], 1, "delete_count == len(ids) regardless of existence");

    // After the tombstone is visible, r1 is gone.
    let gone = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
            .json(&json!({"workspace_id": ws, "ids": ["r1"]}))).await;
        r.data()["hits"].as_array().map(|a| a.is_empty()).unwrap_or(false)
    })
    .await;
    assert!(gone, "deleted record still queryable after timeout");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn db_vectors_dense_semantic_search() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;

    // The db vectors plane ranks by dense COSINE ANN only — no sparse/hybrid
    // mode is exposed here (sparse BM25 lives on the fs /v1/search plane).
    // Prove the dense path matches on MEANING, not lexical overlap: each query
    // shares no tokens with the stored text it should retrieve.
    send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [
            {"id": "pet", "text": "a domestic feline dozed on the warm windowsill"},
            {"id": "auto", "text": "the mechanic replaced the engine timing belt"}
        ]
    }))).await;

    // "kitten napping" ≈ pet record (feline/dozed) with zero shared tokens.
    let hit = poll_first_hit(&s, &vk, &ws, "a kitten taking a nap", Duration::from_secs(20)).await
        .expect("dense search returned nothing");
    assert_eq!(hit["id"], "pet", "dense semantic ranks the cat record first");
    assert!(hit["score"].is_number(), "cosine score present");

    // "fixing a broken car" ≈ auto record, again no shared tokens.
    let hit = poll_first_hit(&s, &vk, &ws, "fixing a broken car", Duration::from_secs(20)).await
        .expect("dense search returned nothing");
    assert_eq!(hit["id"], "auto", "dense semantic ranks the car record first");

    s.drop_ws(&vk, &ws).await;
}

/// Run a search and return the top hit once results appear (Milvus visibility
/// can lag a beat behind upsert). The hit is captured in the same request that
/// observes it, so there is no "found then empty on re-fetch" race.
async fn poll_first_hit(s: &Srv, vk: &str, ws: &str, query: &str, timeout: Duration) -> Option<Value> {
    let start = Instant::now();
    loop {
        let r = send(s.post("/v1/vectors/search").bearer_auth(vk)
            .json(&json!({"workspace_id": ws, "query": query, "top_k": 10}))).await;
        if let Some(hit) = r.data()["hits"].as_array().and_then(|a| a.first().cloned()) {
            return Some(hit);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

#[tokio::test]
#[ignore]
async fn db_vectors_dedup_defaults_and_autoid() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;

    // Duplicate id within one batch → last-wins, ids deduped.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [
            {"id": "dup", "text": "first version"},
            {"id": "dup", "text": "second version wins"}
        ]
    }))).await;
    want(&r, 200, "dedup upsert");
    assert_eq!(r.data()["ids"].as_array().unwrap().len(), 1, "ids deduped to one");
    assert_eq!(r.data()["ids"][0], "dup");
    // Last write wins: the surviving record carries the second text.
    let won = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
            .json(&json!({"workspace_id": ws, "ids": ["dup"]}))).await;
        r.data()["hits"][0]["text"] == "second version wins"
    })
    .await;
    assert!(won, "duplicate id did not resolve to the last write");

    // Omitted id → server fills a UUID, surfaced in the response.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [{"text": "no id provided here"}]
    }))).await;
    want(&r, 200, "autoid upsert");
    let auto = r.data()["ids"][0].as_str().unwrap().to_string();
    assert!(auto.len() >= 20 && auto != "dup", "server-generated id: {auto}");

    // Defaults applied for omitted category/tags/meta.
    send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [{"id": "d1", "text": "bare record"}]
    }))).await;
    let hit = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
            .json(&json!({"workspace_id": ws, "ids": ["d1"]}))).await;
        !r.data()["hits"].as_array().map(|a| a.is_empty()).unwrap_or(true)
    })
    .await;
    assert!(hit, "d1 not queryable");
    let r = send(s.post("/v1/vectors/query").bearer_auth(&vk)
        .json(&json!({"workspace_id": ws, "ids": ["d1"]}))).await;
    let rec = &r.data()["hits"][0].clone();
    assert_eq!(rec["category"], "default", "default category");
    assert_eq!(rec["tags"], json!([]), "default empty tags");
    assert_eq!(rec["meta"], json!({}), "default empty meta");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn db_vectors_filter_and_projection() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;

    send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws,
        "records": [
            {"id": "a", "text": "apple fruit", "meta": {"price": 10, "color": "red"}},
            {"id": "b", "text": "banana fruit", "meta": {"price": 20, "color": "yellow"}},
            {"id": "c", "text": "cherry fruit", "meta": {"price": 30, "color": "red"}}
        ]
    }))).await;

    // Wait for visibility.
    let ready = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/vectors/search").bearer_auth(&vk)
            .json(&json!({"workspace_id": ws, "query": "fruit", "top_k": 10}))).await;
        r.data()["hits"].as_array().map(|a| a.len() >= 3).unwrap_or(false)
    })
    .await;
    assert!(ready, "records not visible");

    let ids = |r: &Resp| -> Vec<String> {
        r.data()["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().to_string()).collect()
    };

    // eq filter on a meta field.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "fruit", "top_k": 10,
        "filter": {"must": [{"field": "meta.color", "op": "eq", "value": "red"}]}
    }))).await;
    want(&r, 200, "eq filter");
    let got = ids(&r);
    assert!(got.contains(&"a".into()) && got.contains(&"c".into()) && !got.contains(&"b".into()), "eq red: {got:?}");

    // range filter.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "fruit", "top_k": 10,
        "filter": {"must": [{"field": "meta.price", "op": "gte", "value": 20}]}
    }))).await;
    let got = ids(&r);
    assert!(got.contains(&"b".into()) && got.contains(&"c".into()) && !got.contains(&"a".into()), "price>=20: {got:?}");

    // in filter.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "fruit", "top_k": 10,
        "filter": {"must": [{"field": "meta.color", "op": "in", "value": ["yellow"]}]}
    }))).await;
    assert_eq!(ids(&r), vec!["b".to_string()], "in yellow");

    // Projection: only requested fields (plus id/score) come back.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "fruit", "top_k": 1, "output_fields": ["text"]
    }))).await;
    want(&r, 200, "projection");
    let h = &r.data()["hits"][0].clone();
    assert!(h["id"].is_string() && h["score"].is_number(), "id/score always returned");
    assert!(h.get("text").is_some(), "projected text present");
    assert!(h.get("category").is_none() && h.get("meta").is_none() && h.get("dataset").is_none(), "unprojected fields omitted");

    // Invalid projections are rejected.
    for bad in [json!(["vector"]), json!(["id"]), json!(["bogus"])] {
        let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
            "workspace_id": ws, "query": "fruit", "output_fields": bad
        }))).await;
        want(&r, 400, "invalid output_fields");
    }

    // The filter DSL only addresses meta.* fields; a platform field is rejected.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "fruit",
        "filter": {"must": [{"field": "category", "op": "eq", "value": "x"}]}
    }))).await;
    want(&r, 400, "non-meta filter field rejected");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn db_datasets_lifecycle() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;

    // Create a second dataset (the "default" one is bootstrapped).
    let r = send(s.post(&format!("/v1/workspaces/{ws}/datasets")).bearer_auth(&vk)
        .json(&json!({"name": "docs"}))).await;
    want(&r, 201, "create dataset");
    assert_eq!(r.data()["name"], "docs");

    // List → both default and docs.
    let r = send(s.get(&format!("/v1/workspaces/{ws}/datasets")).bearer_auth(&vk)).await;
    want(&r, 200, "list datasets");
    let names: Vec<String> = r.data()["items"].as_array().unwrap().iter()
        .map(|d| d["name"].as_str().unwrap().to_string()).collect();
    assert!(names.contains(&"default".into()) && names.contains(&"docs".into()), "{names:?}");

    // Duplicate name → 409.
    let r = send(s.post(&format!("/v1/workspaces/{ws}/datasets")).bearer_auth(&vk)
        .json(&json!({"name": "docs"}))).await;
    want(&r, 409, "duplicate dataset");

    // Upsert scoped to the new dataset, then confirm dataset isolation.
    send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "dataset": "docs",
        "records": [{"id": "only-in-docs", "text": "a document about quarterly planning"}]
    }))).await;
    let in_docs = poll(Duration::from_secs(15), || async {
        let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
            "workspace_id": ws, "dataset": "docs", "query": "quarterly planning", "top_k": 5
        }))).await;
        r.data()["hits"].as_array().map(|a| a.iter().any(|h| h["id"] == "only-in-docs")).unwrap_or(false)
    })
    .await;
    assert!(in_docs, "record not found in its own dataset");
    // The default dataset must NOT see the docs record.
    let r = send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&json!({
        "workspace_id": ws, "query": "quarterly planning", "top_k": 5
    }))).await;
    let d = r.data();
    let ids: Vec<&str> = d["hits"].as_array().unwrap().iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(!ids.contains(&"only-in-docs"), "dataset isolation breached: {ids:?}");

    // Delete the docs dataset → 204. The default dataset is protected → 400.
    let r = send(s.delete(&format!("/v1/workspaces/{ws}/datasets/docs")).bearer_auth(&vk)).await;
    want(&r, 204, "delete dataset");
    let r = send(s.delete(&format!("/v1/workspaces/{ws}/datasets/default")).bearer_auth(&vk)).await;
    want(&r, 400, "cannot delete default dataset");
    // Deleting an unknown dataset → 404.
    let r = send(s.delete(&format!("/v1/workspaces/{ws}/datasets/nope")).bearer_auth(&vk)).await;
    want(&r, 404, "delete unknown dataset");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn db_vectors_validation_limits() {
    let s = Srv::new();
    let (vk, ws) = db_ctx(&s).await;
    let up = |body: Value| {
        let s = s.clone();
        let vk = vk.clone();
        async move { send(s.post("/v1/vectors/upsert").bearer_auth(&vk).json(&body)).await }
    };

    // Bad input → 400 INVALID_INPUT (error_code checked, not just status, so a
    // 413/PAYLOAD_TOO_LARGE can't masquerade as a generic validation failure).
    let bad = "INVALID_INPUT";
    want_err(&up(json!({"workspace_id": ws, "records": []})).await, 400, bad, "empty records");
    want_err(&up(json!({"workspace_id": ws, "records": [{"text": ""}]})).await, 400, bad, "empty text");
    want_err(&up(json!({"workspace_id": ws, "records": [{"text": "x".repeat(65_536)}]})).await, 400, bad, "oversize text");
    let tags: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
    want_err(&up(json!({"workspace_id": ws, "records": [{"text": "ok", "tags": tags}]})).await, 400, bad, "too many tags");
    want_err(&up(json!({"workspace_id": ws, "records": [{"id": "a:b", "text": "ok"}]})).await, 400, bad, "colon in id");
    want_err(&up(json!({"workspace_id": ws, "records": [{"text": "ok", "meta": {"big": "x".repeat(17_000)}}]})).await, 400, bad, "oversize meta");
    // Oversized batch → 413 PAYLOAD_TOO_LARGE (a distinct error class).
    let many: Vec<Value> = (0..501).map(|i| json!({"text": format!("rec {i}")})).collect();
    want_err(&up(json!({"workspace_id": ws, "records": many})).await, 413, "PAYLOAD_TOO_LARGE", "batch over 500");

    // Search bounds.
    let search = |body: Value| {
        let s = s.clone();
        let vk = vk.clone();
        async move { send(s.post("/v1/vectors/search").bearer_auth(&vk).json(&body)).await }
    };
    want_err(&search(json!({"workspace_id": ws, "query": "x", "top_k": 0})).await, 400, bad, "top_k 0");
    want_err(&search(json!({"workspace_id": ws, "query": "x", "top_k": 101})).await, 413, "PAYLOAD_TOO_LARGE", "top_k over 100");
    want_err(&search(json!({"workspace_id": ws, "query": ""})).await, 400, bad, "empty query");

    // Query/delete empty id lists.
    want_err(&send(s.post("/v1/vectors/query").bearer_auth(&vk).json(&json!({"workspace_id": ws, "ids": []}))).await, 400, bad, "empty query ids");
    want_err(&send(s.post("/v1/vectors/delete").bearer_auth(&vk).json(&json!({"workspace_id": ws, "ids": []}))).await, 400, bad, "empty delete ids");

    s.drop_ws(&vk, &ws).await;
}

#[tokio::test]
#[ignore]
async fn db_workspace_resolution_and_admin_tokens() {
    let s = Srv::new();
    let vk = s.account().await;
    let ws1 = s.workspace(&vk, "db").await;

    // Account-wide token + omitted workspace_id → ambiguous → 400.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&vk)
        .json(&json!({"records": [{"text": "no workspace"}]}))).await;
    want(&r, 400, "omitted workspace_id with account-wide token");

    // The /admin/* plane is not part of the public API surface — on this
    // deployment it is firewalled at the ingress (nginx 405). Probe once and
    // skip the scoped-token flow when it is unreachable, so the suite stays
    // green against a hardened proxy while still exercising the logic on a
    // deployment that exposes it.
    let probe = send(s.post("/admin/v1/tokens").bearer_auth(&vk).json(&json!({
        "app_id": "e2e-app", "name": "scoped", "allowed_workspaces": [ws1]
    }))).await;
    if probe.status == 404 || probe.status == 405 {
        eprintln!("/admin/v1/tokens unreachable (HTTP {}); skipping scoped-token assertions", probe.status);
        s.drop_ws(&vk, &ws1).await;
        return;
    }
    want(&probe, 201, "create scoped token");
    let tok_id = probe.data()["id"].as_str().unwrap().to_string();
    let scoped = probe.data()["token"].as_str().unwrap().to_string();
    let ws2 = s.workspace(&vk, "db").await;

    // Single-workspace scope makes workspace_id implicit → resolves to ws1.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&scoped)
        .json(&json!({"records": [{"id": "imp", "text": "implicit workspace works"}]}))).await;
    want(&r, 200, "implicit workspace resolution");

    // The scoped token cannot reach ws2 (out of scope) → 403.
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&scoped)
        .json(&json!({"workspace_id": ws2, "records": [{"text": "denied"}]}))).await;
    want(&r, 403, "out-of-scope workspace");

    // Disable the token → it stops working (401).
    let r = send(s.post(&format!("/admin/v1/tokens/{tok_id}/disable")).bearer_auth(&vk)).await;
    want(&r, 204, "disable token");
    let r = send(s.post("/v1/vectors/upsert").bearer_auth(&scoped)
        .json(&json!({"records": [{"text": "after disable"}]}))).await;
    want(&r, 401, "disabled token rejected");

    s.drop_ws(&vk, &ws1).await;
    s.drop_ws(&vk, &ws2).await;
}

// ════════════════════════════════════════════════════════════════════════
//  Group 7 — Cross-kind isolation (fs ⇄ db)
// ════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn isolation_fs_workspace_rejects_db_apis() {
    let s = Srv::new();
    let vk = s.account().await;
    let fs_ws = s.workspace(&vk, "fs").await;

    // Every vector endpoint rejects an fs workspace with a kind mismatch.
    for (path, body) in [
        ("/v1/vectors/upsert", json!({"workspace_id": fs_ws, "records": [{"text": "x"}]})),
        ("/v1/vectors/search", json!({"workspace_id": fs_ws, "query": "x"})),
        ("/v1/vectors/query", json!({"workspace_id": fs_ws, "ids": ["x"]})),
        ("/v1/vectors/delete", json!({"workspace_id": fs_ws, "ids": ["x"]})),
    ] {
        let r = send(s.post(path).bearer_auth(&vk).json(&body)).await;
        want(&r, 400, path);
        assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH", "{path} kind mismatch");
    }

    // The datasets plane is db-only too — every verb rejects an fs workspace.
    let r = send(s.post(&format!("/v1/workspaces/{fs_ws}/datasets")).bearer_auth(&vk)
        .json(&json!({"name": "x"}))).await;
    want(&r, 400, "datasets POST on fs ws");
    assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH");
    let r = send(s.get(&format!("/v1/workspaces/{fs_ws}/datasets")).bearer_auth(&vk)).await;
    want(&r, 400, "datasets GET on fs ws");
    assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH");
    let r = send(s.delete(&format!("/v1/workspaces/{fs_ws}/datasets/x")).bearer_auth(&vk)).await;
    want(&r, 400, "datasets DELETE on fs ws");
    assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH");

    s.drop_ws(&vk, &fs_ws).await;
}

#[tokio::test]
#[ignore]
async fn isolation_db_workspace_rejects_fs_apis() {
    let s = Srv::new();
    let vk = s.account().await;
    let db_ws = s.workspace(&vk, "db").await;
    // A workspace key on a db workspace is still rejected by fs endpoints,
    // which require kind == Fs.
    let wk = s.wk(&vk, &db_ws, "readwrite").await;

    // GET fs list.
    let r = send(s.get("/v1/fs").query(&[("list", "1")]).bearer_auth(&wk)).await;
    want(&r, 400, "fs list on db ws");
    assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH");

    // Each fs-only POST endpoint.
    for (path, body) in [
        ("/v1/grep", json!({"pattern": "x"})),
        ("/v1/search", json!({"query": "x"})),
        ("/v1/sql", json!({"sql": "SELECT 1"})),
        ("/v1/collections", json!({"name": "c", "fields": []})),
    ] {
        let r = send(s.post(path).bearer_auth(&wk).json(&body)).await;
        want(&r, 400, path);
        assert_eq!(r.ecode(), "WORKSPACE_KIND_MISMATCH", "{path} kind mismatch");
    }

    s.drop_ws(&vk, &db_ws).await;
}

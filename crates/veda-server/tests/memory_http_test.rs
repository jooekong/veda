//! Memory surface end-to-end (docs/plans/agent-memory-m1.md Step 4): REST +
//! MCP against real MySQL/Milvus/embedding, including the two GateMem
//! assertions as deterministic checks —
//!   1. cross-domain recall = 0 (another principal, another workspace)
//!   2. deleted recall = 0 (API delete AND a hand-made Milvus-remnant window)
//!
//! Run: NO_PROXY='*' cargo test -p veda-server --test memory_http_test -- --ignored --test-threads=1

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use veda_core::checksum::sha256_hex;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, MemoryVectorStore, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::state::AppState;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, KeyPermission, KeyStatus, Workspace, WorkspaceKey, WorkspaceKind,
    WorkspaceStatus,
};

#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: MysqlSection,
    milvus: MilvusSection,
    embedding: EmbeddingSection,
}
#[derive(Debug, Deserialize)]
struct MysqlSection {
    database_url: String,
}
#[derive(Debug, Deserialize)]
struct MilvusSection {
    url: String,
    token: Option<String>,
    db: Option<String>,
}
#[derive(Debug, Deserialize)]
struct EmbeddingSection {
    api_url: String,
    api_key: String,
    model: String,
    dimension: u32,
    #[serde(default = "default_batch_size")]
    batch_size: usize,
}
fn default_batch_size() -> usize {
    10
}

fn load_config() -> TestConfig {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("config/test.toml");
    toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn test_metrics() -> veda_server::obs::MetricsHandle {
    use std::sync::OnceLock;
    static METRICS: OnceLock<veda_server::obs::MetricsHandle> = OnceLock::new();
    METRICS.get_or_init(veda_server::obs::install).clone()
}

struct TestApp {
    state: Arc<AppState>,
    mysql: Arc<MysqlStore>,
    router: Router,
}

async fn build_app() -> TestApp {
    build_app_with(None).await
}

async fn build_app_with(
    directory: Option<Arc<dyn veda_core::store::PersonDirectory>>,
) -> TestApp {
    let cfg = load_config();
    let mysql = Arc::new(
        MysqlStore::with_pool_config(
            &cfg.mysql.database_url,
            PoolConfig {
                max_connections: 20,
                ..Default::default()
            },
        )
        .await
        .expect("mysql connect"),
    );
    mysql.migrate().await.expect("migrate");
    let milvus = Arc::new(MilvusStore::new(
        &cfg.milvus.url,
        cfg.milvus.token.clone(),
        cfg.milvus.db.clone(),
    ));
    let embedding = Arc::new(
        EmbeddingProvider::new_tuned(
            &cfg.embedding.api_url,
            &cfg.embedding.api_key,
            &cfg.embedding.model,
            cfg.embedding.dimension,
            cfg.embedding.batch_size,
            8,
        )
        .expect("embedding"),
    );
    let vector_embedding: Arc<dyn EmbeddingService> =
        Arc::new(EmbeddingCache::new(embedding.clone(), &cfg.embedding.model));
    milvus
        .init_collections(cfg.embedding.dimension)
        .await
        .expect("init_collections");
    MemoryVectorStore::init_memory_collection(milvus.as_ref(), cfg.embedding.dimension)
        .await
        .expect("init memory collection");

    let fs_service = Arc::new(FsService::new(mysql.clone()));
    let search_service = SearchService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let collection_service =
        CollectionService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let sql_engine = veda_sql::VedaSqlEngine::new(
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        fs_service.clone(),
    );
    let state = Arc::new(AppState {
        fs_service,
        search_service,
        collection_service,
        auth_store: mysql.clone(),
        meta_store: mysql.clone(),
        vector_store: milvus.clone(),
        reconciler: Arc::new(veda_server::reconciler::Reconciler::new(
            mysql.clone(),
            milvus.clone(),
            mysql.clone(),
        )),
        vector_workspace_store: milvus.clone(),
        vector_service: veda_core::service::vector::VectorService::new(
            milvus.clone(),
            vector_embedding.clone(),
            mysql.clone(),
        ),
        workspace_service: veda_core::service::workspace::WorkspaceService::new(
            mysql.clone(),
            milvus.clone(),
            cfg.embedding.dimension,
        ),
        vector_embedding,
        sql_engine,
        metrics: test_metrics(),
        metrics_token: None,
        admin_token: None,
        memory_service: Arc::new(veda_core::service::memory::MemoryService::new(
            mysql.clone(),
            milvus.clone(),
            embedding.clone(),
            directory,
        )),
        summary_enabled: false,
        answer_service: None,
        answer_concurrency: 2,
        tunnel_bots: Arc::new(
            veda_server::tunnel_bots::TunnelBotStore::connect(&cfg.mysql.database_url)
                .await
                .expect("tunnel bots"),
        ),
        access_recorder: Arc::new(veda_core::service::access_stats::AccessRecorder::new(
            mysql.clone(),
            8,
            false,
        )),
        draining: std::sync::atomic::AtomicBool::new(false),
    });
    let router = veda_server::routes::build_router(state.clone());
    TestApp {
        state,
        mysql,
        router,
    }
}

// ── provisioning ─────────────────────────────────────────

struct WsSetup {
    acct_id: String,
    ws_id: String,
    wk: String,
}

async fn provision(state: &AppState) -> WsSetup {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "memory-test".into(),
            email: Some(format!("{}@memory-test.com", &acct_id[..8])),
            password_hash: None,
            app_id: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let ws_id = Uuid::new_v4().to_string();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "memory-ws".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Fs,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let wk = mint_key(state, &acct_id, &ws_id, KeyPermission::ReadWrite).await;
    WsSetup {
        acct_id,
        ws_id,
        wk,
    }
}

async fn mint_key(
    state: &AppState,
    acct_id: &str,
    ws_id: &str,
    permission: KeyPermission,
) -> String {
    let raw = format!("wk_{}", Uuid::new_v4().simple());
    state
        .auth_store
        .create_workspace_key(&WorkspaceKey {
            id: Uuid::new_v4().to_string(),
            workspace_id: ws_id.to_string(),
            account_id: acct_id.to_string(),
            name: "memory-test-wk".into(),
            key_hash: sha256_hex(raw.as_bytes()),
            permission,
            status: KeyStatus::Active,
            kind: WorkspaceKind::Fs,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    raw
}

// ── request helpers ──────────────────────────────────────

async fn send(
    router: Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send_as(router, method, uri, token, None, body).await
}

/// Like `send` but with an `X-Veda-Operator` assertion (M3a).
async fn send_as(
    router: Router,
    method: &str,
    uri: &str,
    token: &str,
    operator: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if let Some(op) = operator {
        b = b.header("x-veda-operator", op);
    }
    let request = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn mcp_call(router: Router, token: &str, name: &str, args: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn items(v: &Value) -> &Vec<Value> {
    v["data"]["items"].as_array().expect("items array")
}

fn contents(v: &Value) -> Vec<String> {
    items(v)
        .iter()
        .map(|i| i["content"].as_str().unwrap().to_string())
        .collect()
}

// ── the mega test ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memory_rest_mcp_and_gatemem_assertions() {
    let app = build_app().await;
    let ws_a = provision(&app.state).await; // person A's key
    let wk_b = mint_key(&app.state, &ws_a.acct_id, &ws_a.ws_id, KeyPermission::ReadWrite).await;
    let wk_ro = mint_key(&app.state, &ws_a.acct_id, &ws_a.ws_id, KeyPermission::Read).await;
    let ws_c = provision(&app.state).await; // an unrelated workspace

    // Distinctive markers keep assertions immune to leftover rows.
    let run = &Uuid::new_v4().simple().to_string()[..8];
    let team_fact = format!("[{run}] integration tests must pass NO_PROXY and single thread");
    let personal_note = format!("[{run}] my private note: the staging box password is in the vault");

    // 1. save team memory via REST
    let (st, resp) = send(
        app.router.clone(),
        "POST",
        "/v1/memory",
        &ws_a.wk,
        Some(json!({ "content": team_fact, "scope": "team", "kind": "fact", "topic": "testing" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    assert_eq!(resp["data"]["memory"]["scope"], "team");
    assert_eq!(resp["data"]["duplicate"], false);
    let team_id = resp["data"]["memory"]["id"].as_i64().unwrap();

    // 2. identical save is a flagged duplicate of the same row
    let (st, resp) = send(
        app.router.clone(),
        "POST",
        "/v1/memory",
        &ws_a.wk,
        Some(json!({ "content": team_fact, "scope": "team" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(resp["data"]["duplicate"], true);
    assert_eq!(resp["data"]["memory"]["id"].as_i64().unwrap(), team_id);

    // 3. personal (mine) memory: preference defaults to portable
    let (st, resp) = send(
        app.router.clone(),
        "POST",
        "/v1/memory",
        &ws_a.wk,
        Some(json!({ "content": personal_note, "kind": "preference" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    assert_eq!(resp["data"]["memory"]["scope"], "mine");
    assert!(resp["data"]["memory"]["origin_workspace_id"].is_null());
    let personal_id = resp["data"]["memory"]["id"].as_i64().unwrap();

    // 4. context for A sees both domains
    let (st, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/context?query=integration%20tests%20{run}&limit=20"),
        &ws_a.wk,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    let got = contents(&resp);
    assert!(got.iter().any(|c| c == &team_fact), "team fact in context: {got:?}");
    assert!(
        got.iter().any(|c| c == &personal_note),
        "own personal note in context: {got:?}"
    );

    // 5. read-only key: reads fine, writes 403
    let (st, _) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/search?query={run}"),
        &wk_ro,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(
        app.router.clone(),
        "POST",
        "/v1/memory",
        &wk_ro,
        Some(json!({ "content": "should not land" })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "read-only key must not write");

    // 6. GateMem #1a — another person's key (B, same workspace): team yes,
    //    A's personal domain never.
    let (st, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/context?query={run}%20private%20note%20staging%20password&limit=50"),
        &wk_b,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let got = contents(&resp);
    assert!(
        !got.iter().any(|c| c == &personal_note),
        "GateMem#1a violated — B can read A's personal memory: {got:?}"
    );
    assert!(
        got.iter().any(|c| c == &team_fact),
        "B should still see the team memory: {got:?}"
    );

    // 7. GateMem #1b — unrelated workspace C sees nothing of A's
    let (st, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/search?query={run}%20integration%20tests&limit=50"),
        &ws_c.wk,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let got = contents(&resp);
    assert!(
        got.iter().all(|c| !c.contains(run)),
        "GateMem#1b violated — workspace C sees A's memories: {got:?}"
    );

    // 8. wiki-style edit: B rewrites the team fact, signature moves
    let corrected = format!("[{run}] integration tests: SQL suites need multi_thread, rest single");
    let (st, resp) = send(
        app.router.clone(),
        "PATCH",
        &format!("/v1/memory/{team_id}"),
        &wk_b,
        Some(json!({ "content": corrected })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    assert_ne!(
        resp["data"]["updated_by"], resp["data"]["created_by"],
        "edit must be signed by the editor"
    );
    // recheck read serves the corrected text immediately
    let (_, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/search?query=integration%20tests%20{run}&limit=20"),
        &ws_a.wk,
        None,
    )
    .await;
    let got = contents(&resp);
    assert!(got.iter().any(|c| c == &corrected), "updated text served: {got:?}");
    assert!(!got.iter().any(|c| c == &team_fact), "old text gone: {got:?}");

    // 9. B cannot update or delete A's personal memory (404, not 403 — the
    //    row must not even be acknowledged)
    let (st, _) = send(
        app.router.clone(),
        "PATCH",
        &format!("/v1/memory/{personal_id}"),
        &wk_b,
        Some(json!({ "content": "hijacked" })),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(
        app.router.clone(),
        "DELETE",
        &format!("/v1/memory/{personal_id}"),
        &wk_b,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // 10. MCP surface: 12 tools listed, save + context roundtrip, read-only
    //     write rejected as a domain error
    let resp = mcp_call(
        app.router.clone(),
        &ws_a.wk,
        "memory_save",
        json!({ "content": format!("[{run}] saved via mcp: the deploy box needs TZ=UTC"), "scope": "team" }),
    )
    .await;
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let mcp_saved: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let mcp_id = mcp_saved["memory"]["id"].as_i64().unwrap();

    let list = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", ws_a.wk))
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" }).to_string(),
        ))
        .unwrap();
    let resp = app.router.clone().oneshot(list).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let listed: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 12);

    let resp = mcp_call(
        app.router.clone(),
        &ws_a.wk,
        "memory_context",
        json!({ "query": format!("deploy box timezone {run}"), "limit": 20 }),
    )
    .await;
    assert_eq!(resp["result"]["isError"], false);
    let ctx_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(ctx_text.contains("TZ=UTC"), "mcp context finds mcp-saved memory: {ctx_text}");

    let resp = mcp_call(
        app.router.clone(),
        &wk_ro,
        "memory_save",
        json!({ "content": "should be rejected" }),
    )
    .await;
    assert_eq!(resp["result"]["isError"], true);
    assert!(resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("read-only"));

    // 11. GateMem #2a — API delete is immediately gone from retrieval
    let (st, _) = send(
        app.router.clone(),
        "DELETE",
        &format!("/v1/memory/{mcp_id}"),
        &ws_a.wk,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/search?query=deploy%20box%20timezone%20{run}&limit=50"),
        &ws_a.wk,
        None,
    )
    .await;
    assert!(
        !contents(&resp).iter().any(|c| c.contains("TZ=UTC")),
        "GateMem#2a violated — deleted memory still retrieved"
    );

    // 12. GateMem #2b — the hand-made remnant window: kill the MySQL row
    //     directly, LEAVING the Milvus vector in place. The recheck read
    //     must still refuse to serve it. This is the assertion that pins
    //     "MySQL recheck is the authority" (plan §0 adjustment 1).
    let remnant = format!("[{run}] remnant: this row will lose its MySQL half");
    let (st, resp) = send(
        app.router.clone(),
        "POST",
        "/v1/memory",
        &ws_a.wk,
        Some(json!({ "content": remnant, "scope": "team" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let remnant_id = resp["data"]["memory"]["id"].as_i64().unwrap();
    sqlx::query("DELETE FROM veda_memories WHERE id = ?")
        .bind(remnant_id)
        .execute(app.mysql.pool())
        .await
        .unwrap();
    let (_, resp) = send(
        app.router.clone(),
        "GET",
        &format!("/v1/memory/search?query=remnant%20mysql%20half%20{run}&limit=50"),
        &ws_a.wk,
        None,
    )
    .await;
    assert!(
        !contents(&resp).iter().any(|c| c == &remnant),
        "GateMem#2b violated — Milvus remnant served without a MySQL row"
    );

    // ── cleanup ──
    let pool = app.mysql.pool();
    for ws in [&ws_a.ws_id, &ws_c.ws_id] {
        let _ = sqlx::query("DELETE FROM veda_memories WHERE scope_id = ?")
            .bind(ws)
            .execute(pool)
            .await;
    }
    // personal rows hang off lazily created principals — find them by key id
    let _ = sqlx::query(
        "DELETE m FROM veda_memories m JOIN veda_principals p ON m.scope_id = p.id \
         JOIN veda_workspace_keys k ON p.external_id = k.id \
         WHERE k.workspace_id IN (?, ?)",
    )
    .bind(&ws_a.ws_id)
    .bind(&ws_c.ws_id)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE p FROM veda_principals p JOIN veda_workspace_keys k ON p.external_id = k.id \
         WHERE k.workspace_id IN (?, ?)",
    )
    .bind(&ws_a.ws_id)
    .bind(&ws_c.ws_id)
    .execute(pool)
    .await;
    for s in [&ws_a, &ws_c] {
        let _ = app.state.auth_store.hard_delete_workspace(&s.ws_id).await;
        let _ = sqlx::query("DELETE FROM veda_workspace_keys WHERE workspace_id = ?")
            .bind(&s.ws_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
            .bind(&s.acct_id)
            .execute(pool)
            .await;
    }
}

// ── M3a: operator identity / dept domain / hybrid / scope move ──

/// Query-string encoding for CJK test payloads (percent-encoding is already
/// a veda-server dependency; no extra dev-dep).
fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Trait-level directory stub: everything except people.rs's HTTP glue runs
/// real (the SSO contract isn't final; the glue is Joe's to wire).
struct StaticDirectory(std::collections::HashMap<(String, String), veda_types::PersonProfile>);

#[async_trait::async_trait]
impl veda_core::store::PersonDirectory for StaticDirectory {
    async fn lookup(
        &self,
        source: veda_types::PrincipalSource,
        external_id: &str,
    ) -> veda_types::Result<Option<veda_types::PersonProfile>> {
        Ok(self
            .0
            .get(&(source.as_str().to_string(), external_id.to_string()))
            .cloned())
    }
}

fn profile(emp: &str, name: &str, dept: &str) -> veda_types::PersonProfile {
    veda_types::PersonProfile {
        emp_no: emp.into(),
        display_name: Some(name.into()),
        dept_id: Some(dept.into()),
        dept_name: Some(format!("dept-{dept}")),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn operator_identity_merge_and_dept_domain() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let mut dir = std::collections::HashMap::new();
    let run = Uuid::new_v4().simple().to_string()[..8].to_string();
    let dept_a = format!("DA-{run}");
    let dept_b = format!("DB-{run}");
    let (zhang_wecom, zhang_emp) = (format!("wz-{run}"), format!("e1-{run}"));
    let li_wecom = format!("wl-{run}");
    dir.insert(("wecom".into(), zhang_wecom.clone()), profile(&zhang_emp, "张三", &dept_a));
    dir.insert(("emp".into(), zhang_emp.clone()), profile(&zhang_emp, "张三", &dept_a));
    dir.insert(("wecom".into(), li_wecom.clone()), profile(&format!("e2-{run}"), "李四", &dept_b));
    // emp_no is VARCHAR(32) — the {run} suffix keeps every id under that.
    let app = build_app_with(Some(Arc::new(StaticDirectory(dir)))).await;
    let ws = provision(&app.state).await;
    let zhang_op = format!("wecom:{zhang_wecom}");
    let zhang_emp_op = format!("emp:{zhang_emp}");
    let li_op = format!("wecom:{li_wecom}");

    // 1) 身份合并: a mine-scope save through the wecom entrance …
    let fact = format!("张三的个人笔记 merge-{run}");
    let (st, v) = send_as(
        app.router.clone(), "POST", "/v1/memory", &ws.wk, Some(&zhang_op),
        Some(json!({ "content": fact, "scope": "mine", "origin": "" })),
    ).await;
    assert_eq!(st, StatusCode::OK, "save via wecom entrance: {v}");
    // … is visible through the emp entrance: two identities, one principal.
    let (st, v) = send_as(
        app.router.clone(), "GET",
        &format!("/v1/memory/search?query={}&scope=mine", enc(&fact)),
        &ws.wk, Some(&zhang_emp_op), None,
    ).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        contents(&v).iter().any(|c| c == &fact),
        "emp entrance must see the wecom-entrance personal memory: {v}"
    );

    // 2) dept domain: 张三 seeds a dept-A memory.
    let dept_fact = format!("部门约定 dept-{run}: 周会挪到周二上午");
    let (st, _) = send_as(
        app.router.clone(), "POST", "/v1/memory", &ws.wk, Some(&zhang_op),
        Some(json!({ "content": dept_fact, "scope": "dept" })),
    ).await;
    assert_eq!(st, StatusCode::OK);

    // GateMem extension: same workspace, other dept — invisible in both
    // explicit dept search and the context union.
    for uri in [
        format!("/v1/memory/search?query={}&scope=dept", enc(&dept_fact)),
        format!("/v1/memory/context?query={}", enc(&dept_fact)),
    ] {
        let (st, v) = send_as(app.router.clone(), "GET", &uri, &ws.wk, Some(&li_op), None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            !contents(&v).iter().any(|c| c == &dept_fact),
            "dept-B operator must not see dept-A memory via {uri}: {v}"
        );
    }

    // Dept memories follow the person across workspaces.
    let ws2 = provision(&app.state).await;
    let (st, v) = send_as(
        app.router.clone(), "GET",
        &format!("/v1/memory/search?query={}&scope=dept", enc(&dept_fact)),
        &ws2.wk, Some(&zhang_op), None,
    ).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        contents(&v).iter().any(|c| c == &dept_fact),
        "dept memory must be visible from another workspace: {v}"
    );

    // 3) dept save without a resolvable dept (no directory entry) → 400.
    let (st, _) = send_as(
        app.router.clone(), "POST", "/v1/memory", &ws.wk, Some(&format!("wecom:ghost-{run}")),
        Some(json!({ "content": "x", "scope": "dept" })),
    ).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "ghost operator has no dept");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn hybrid_keyword_recall_and_scope_move() {
    let app = build_app().await;
    let ws = provision(&app.state).await;
    let run = Uuid::new_v4().simple().to_string();

    // BM25 lane: a rare exact token buried in an otherwise generic line.
    let token = format!("XK9Q7Z{}", &run[..6]);
    let fact = format!("内部代号 {token} 的服务经 .85 的 SSH 隧道访问生产 MySQL");
    let (st, _) = send(
        app.router.clone(), "POST", "/v1/memory", &ws.wk,
        Some(json!({ "content": fact, "scope": "team" })),
    ).await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = send(
        app.router.clone(), "GET",
        &format!("/v1/memory/search?query={token}&scope=team"),
        &ws.wk, None,
    ).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        contents(&v).iter().any(|c| c.contains(&token)),
        "exact-token query must recall via the BM25 lane: {v}"
    );

    // Scope move (mine → team), content unchanged: the target domain must
    // see it immediately and the source domain must not (Milvus scalars
    // refreshed — the R6 regression).
    let note = format!("个人踩坑 move-{run}: k6 要加 SALT");
    let (st, v) = send(
        app.router.clone(), "POST", "/v1/memory", &ws.wk,
        Some(json!({ "content": note, "scope": "mine", "origin": "" })),
    ).await;
    assert_eq!(st, StatusCode::OK);
    let id = v["data"]["memory"]["id"].as_i64().expect("memory id");
    let (st, v) = send(
        app.router.clone(), "PATCH", &format!("/v1/memory/{id}"), &ws.wk,
        Some(json!({ "scope": "team" })),
    ).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["scope"].as_str(), Some("team"), "{v}");
    let (_, team) = send(
        app.router.clone(), "GET",
        &format!("/v1/memory/search?query={}&scope=team", enc(&note)),
        &ws.wk, None,
    ).await;
    assert!(
        items(&team).iter().any(|i| i["id"].as_i64() == Some(id)),
        "moved memory must be searchable in the target domain: {team}"
    );
    let (_, mine) = send(
        app.router.clone(), "GET",
        &format!("/v1/memory/search?query={}&scope=mine", enc(&note)),
        &ws.wk, None,
    ).await;
    assert!(
        !items(&mine).iter().any(|i| i["id"].as_i64() == Some(id)),
        "moved memory must leave the source domain: {mine}"
    );

    // Round-tripping an UNCHANGED scope is not a move: a workspace-pinned
    // personal note must keep its origin (R7 regression — clearing it would
    // leak the note into every other workspace's context).
    let pinned = format!("个人钉住 keep-{run}: 本 ws 的私货");
    let (st, v) = send(
        app.router.clone(), "POST", "/v1/memory", &ws.wk,
        Some(json!({ "content": pinned, "scope": "mine" })),
    ).await;
    assert_eq!(st, StatusCode::OK);
    let pid = v["data"]["memory"]["id"].as_i64().unwrap();
    assert_eq!(v["data"]["memory"]["origin_workspace_id"].as_str(), Some(ws.ws_id.as_str()));
    let (st, v) = send(
        app.router.clone(), "PATCH", &format!("/v1/memory/{pid}"), &ws.wk,
        Some(json!({ "scope": "mine", "topic": "keep" })),
    ).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["data"]["origin_workspace_id"].as_str(),
        Some(ws.ws_id.as_str()),
        "unchanged scope must not clear origin: {v}"
    );
}

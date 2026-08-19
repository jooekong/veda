//! HTTP roundtrip for the MCP endpoint (`POST /mcp`, streamable-http
//! stateless mode).
//!
//! Runs the real `build_router(AppState)` against real MySQL + Milvus +
//! embedding via `tower::ServiceExt::oneshot` (no TCP). One `#[ignore]`d
//! mega-test (sqlx pools are tied to the runtime that created them) covers:
//!   - protocol: initialize version negotiation, tools/list catalogue, ping,
//!     notification → 202, batch / parse / unknown-method / unknown-tool
//!     errors, GET → 405
//!   - auth: missing bearer → 401, db-kind wk_ → 400 (kind mismatch)
//!   - tools against real data: list_dir (flat + recursive), read_file
//!     (whole / line-ranged / missing), grep, search (worker-driven
//!     ChunkSync → Milvus hybrid hit)
//!   - degraded modes: overview with summaries disabled, ask with no [llm]
//!     — both isError=true with actionable text
//!
//! Run with:
//!   NO_PROXY='*' cargo test -p veda-server --test mcp_http_test -- --ignored --test-threads=1

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tower::ServiceExt;
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_server::worker::Worker;
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
    let raw = std::fs::read_to_string(&path).unwrap();
    toml::from_str(&raw).unwrap()
}

fn test_metrics() -> veda_server::obs::MetricsHandle {
    use std::sync::OnceLock;
    static METRICS: OnceLock<veda_server::obs::MetricsHandle> = OnceLock::new();
    METRICS.get_or_init(veda_server::obs::install).clone()
}

struct TestApp {
    state: Arc<AppState>,
    mysql: Arc<MysqlStore>,
    milvus: Arc<MilvusStore>,
    embedding: Arc<EmbeddingProvider>,
    router: Router,
}

/// Real app, summaries + answer deliberately disabled: the MCP suite pins
/// the degraded-mode tool texts for overview/ask (the enabled paths need an
/// LLM and are covered by manual smoke / answer's own tests).
async fn build_app() -> TestApp {
    let cfg = load_config();
    let pool_config = PoolConfig {
        max_connections: 20,
        ..Default::default()
    };
    let mysql = Arc::new(
        MysqlStore::with_pool_config(&cfg.mysql.database_url, pool_config)
            .await
            .expect("mysql connect"),
    );
    mysql.migrate().await.expect("mysql migrate");

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
        memory_service: std::sync::Arc::new(veda_core::service::memory::MemoryService::new(
            mysql.clone(),
            milvus.clone(),
            embedding.clone(),
            None,
        )),
        summary_enabled: false,
        answer_service: None,
        answer_concurrency: 2,
        tunnel_bots: Arc::new(
            veda_server::tunnel_bots::TunnelBotStore::connect(&cfg.mysql.database_url)
                .await
                .expect("tunnel bots store"),
        ),
        access_recorder: Arc::new(veda_core::service::access_stats::AccessRecorder::disabled(
            mysql.clone(),
        )),
        draining: std::sync::atomic::AtomicBool::new(false),
    });
    let router = build_router(state.clone());
    TestApp {
        state,
        mysql,
        milvus,
        embedding,
        router,
    }
}

// ── provisioning (same shapes as admin_http_test) ───────

struct WsSetup {
    acct_id: String,
    ws_id: String,
    wk: String,
}

async fn create_account(state: &AppState) -> String {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "mcp-test".into(),
            email: Some(format!("{}@mcp-test.com", &acct_id[..8])),
            password_hash: None,
            app_id: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    acct_id
}

async fn create_wk(state: &AppState, acct_id: &str, ws_id: &str, kind: WorkspaceKind) -> String {
    let raw = format!("wk_{}", Uuid::new_v4().simple());
    state
        .auth_store
        .create_workspace_key(&WorkspaceKey {
            id: Uuid::new_v4().to_string(),
            workspace_id: ws_id.to_string(),
            account_id: acct_id.to_string(),
            name: "mcp-test-wk".into(),
            key_hash: sha256_hex(raw.as_bytes()),
            permission: KeyPermission::ReadWrite,
            status: KeyStatus::Active,
            kind,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    raw
}

async fn provision_workspace(state: &AppState, kind: WorkspaceKind) -> WsSetup {
    let acct_id = create_account(state).await;
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "mcp-ws".into(),
            status: WorkspaceStatus::Active,
            kind,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let wk = create_wk(state, &acct_id, &ws_id, kind).await;
    WsSetup {
        acct_id,
        ws_id,
        wk,
    }
}

async fn cleanup(state: &AppState, mysql: &MysqlStore, setups: &[&WsSetup]) {
    for s in setups {
        let _ = state.auth_store.hard_delete_workspace(&s.ws_id).await;
        let _ = sqlx::query("DELETE FROM veda_workspace_keys WHERE workspace_id = ?")
            .bind(&s.ws_id)
            .execute(mysql.pool())
            .await;
        let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
            .bind(&s.acct_id)
            .execute(mysql.pool())
            .await;
    }
}

// ── request helpers ──────────────────────────────────────

/// POST a raw body to /mcp with optional bearer; returns (status, parsed body
/// or Null when empty/unparsable).
async fn mcp_raw(router: Router, token: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let request = b.body(Body::from(body.to_string())).unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn rpc(router: Router, token: &str, body: Value) -> Value {
    let (status, resp) = mcp_raw(router, Some(token), &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "rpc http status: {resp}");
    resp
}

fn rpc_req(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

async fn call_tool(router: Router, token: &str, name: &str, args: Value) -> Value {
    rpc(
        router,
        token,
        rpc_req(7, "tools/call", json!({ "name": name, "arguments": args })),
    )
    .await
}

/// Extract result.content[0].text from a tools/call response.
fn tool_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool text in {resp}"))
}

fn tool_is_error(resp: &Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(false)
}

/// PUT a file through the REST surface (the ingestion path MCP consumers
/// pair with).
async fn put_file(router: Router, token: &str, path: &str, content: &str) -> StatusCode {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/fs{path}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(content.to_string()))
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

// ── the suite ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // needs real MySQL/Milvus/embedding — run explicitly with `--ignored`
async fn mcp_http_suite() {
    let app = build_app().await;
    let fs_ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;
    let db_ws = provision_workspace(&app.state, WorkspaceKind::Db).await;
    let wk = fs_ws.wk.as_str();

    // ── auth ──
    let (st, _) = mcp_raw(app.router.clone(), None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no bearer → 401");
    let (st, _) = mcp_raw(
        app.router.clone(),
        Some(&db_ws.wk),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "db-kind wk_ → kind mismatch 400");

    // ── GET /mcp: no downstream SSE stream in stateless mode ──
    let resp = app
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/mcp")
                .header("authorization", format!("Bearer {wk}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "GET /mcp → 405");

    // ── protocol basics ──
    let r = rpc(
        app.router.clone(),
        wk,
        rpc_req(1, "initialize", json!({ "protocolVersion": "2025-06-18" })),
    )
    .await;
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 1);
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18", "echo supported version");
    assert_eq!(r["result"]["serverInfo"]["name"], "veda");
    assert!(r["result"]["capabilities"]["tools"].is_object());

    let r = rpc(
        app.router.clone(),
        wk,
        rpc_req(2, "initialize", json!({ "protocolVersion": "2025-03-26" })),
    )
    .await;
    assert_eq!(
        r["result"]["protocolVersion"], "2025-06-18",
        "unadvertised revision → counter-offer newest"
    );

    // MCP-Protocol-Version header gate: unsupported value → HTTP 400.
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {wk}"))
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-03-26")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#.to_string(),
        ))
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "unsupported protocol-version header → 400"
    );

    // notification (no id) → 202, empty body
    let (st, body) = mcp_raw(
        app.router.clone(),
        Some(wk),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "notification → 202");
    assert_eq!(body, Value::Null, "notification response has no body");

    let r = rpc(app.router.clone(), wk, rpc_req(3, "ping", json!({}))).await;
    assert_eq!(r["result"], json!({}));

    let r = rpc(app.router.clone(), wk, rpc_req(4, "tools/list", json!({}))).await;
    let tools = r["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["layout", "search", "grep", "read_file", "list_dir", "overview", "ask"],
        "tool catalogue"
    );

    // protocol errors
    let (st, r) = mcp_raw(app.router.clone(), Some(wk), "{not json").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(r["error"]["code"], -32700, "parse error");
    let (_, r) = mcp_raw(app.router.clone(), Some(wk), r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
    assert_eq!(r["error"]["code"], -32600, "batch rejected");
    let (_, r) = mcp_raw(app.router.clone(), Some(wk), r#"{"id":1,"method":"ping"}"#).await;
    assert_eq!(r["error"]["code"], -32600, "missing jsonrpc field rejected");
    let (_, r) = mcp_raw(
        app.router.clone(),
        Some(wk),
        r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#,
    )
    .await;
    assert_eq!(r["error"]["code"], -32600, "object id rejected");
    assert_eq!(r["id"], Value::Null, "invalid request → id null");
    // "id": null is a request, not a notification — it gets a response.
    let (st, r) = mcp_raw(
        app.router.clone(),
        Some(wk),
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "id:null is a request");
    assert_eq!(r["id"], Value::Null);
    assert_eq!(r["result"], json!({}));
    let r = rpc(app.router.clone(), wk, rpc_req(5, "resources/list", json!({}))).await;
    assert_eq!(r["error"]["code"], -32601, "unknown method");
    let r = call_tool(app.router.clone(), wk, "no_such_tool", json!({})).await;
    assert_eq!(r["error"]["code"], -32602, "unknown tool");
    let r = rpc(
        app.router.clone(),
        wk,
        rpc_req(6, "tools/call", json!({ "name": "search", "arguments": {} })),
    )
    .await;
    assert_eq!(r["error"]["code"], -32602, "missing required arg");

    // ── seed real files ──
    const SENTINEL: &str = "VEDA_MCP_E2E_SENTINEL_2233";
    let md = format!(
        "# MCP e2e page\n\n定时任务的重试策略说明,哨兵 {SENTINEL} 在这一行。\n\nsecond line for paging.\n"
    );
    assert_eq!(
        put_file(app.router.clone(), wk, "/wiki/mcp-e2e.md", &md).await,
        StatusCode::OK
    );
    assert_eq!(
        put_file(app.router.clone(), wk, "/wiki/other.md", "nothing here\n").await,
        StatusCode::OK
    );

    // ── list_dir ──
    let r = call_tool(app.router.clone(), wk, "list_dir", json!({ "path": "/wiki" })).await;
    assert!(!tool_is_error(&r), "list_dir errored: {r}");
    let listing: Value = serde_json::from_str(tool_text(&r)).unwrap();
    assert_eq!(listing["truncated"], false);
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"mcp-e2e.md"), "flat listing: {names:?}");

    let r = call_tool(
        app.router.clone(),
        wk,
        "list_dir",
        json!({ "recursive": true }),
    )
    .await;
    let listing: Value = serde_json::from_str(tool_text(&r)).unwrap();
    assert_eq!(listing["truncated"], false);
    let paths: Vec<&str> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"/wiki/mcp-e2e.md"), "recursive listing: {paths:?}");

    // ── read_file ──
    let r = call_tool(
        app.router.clone(),
        wk,
        "read_file",
        json!({ "path": "/wiki/mcp-e2e.md" }),
    )
    .await;
    assert!(!tool_is_error(&r));
    assert!(tool_text(&r).contains(SENTINEL), "whole-file read");

    // leading slash optional
    let r = call_tool(
        app.router.clone(),
        wk,
        "read_file",
        json!({ "path": "wiki/mcp-e2e.md", "start_line": 1, "end_line": 1 }),
    )
    .await;
    assert!(!tool_is_error(&r));
    let first = tool_text(&r);
    assert!(first.contains("# MCP e2e page"), "line range read: {first:?}");
    assert!(!first.contains(SENTINEL), "line 1 only");

    let r = call_tool(
        app.router.clone(),
        wk,
        "read_file",
        json!({ "path": "/wiki/nope.md" }),
    )
    .await;
    assert!(tool_is_error(&r), "missing file → isError");
    let r = call_tool(
        app.router.clone(),
        wk,
        "read_file",
        json!({ "path": "/wiki/mcp-e2e.md", "start_line": 0 }),
    )
    .await;
    assert_eq!(r["error"]["code"], -32602, "start_line 0 → invalid params");

    // ── grep ──
    let r = call_tool(
        app.router.clone(),
        wk,
        "grep",
        json!({ "pattern": SENTINEL }),
    )
    .await;
    assert!(!tool_is_error(&r));
    let hits: Value = serde_json::from_str(tool_text(&r)).unwrap();
    let hit = &hits.as_array().unwrap()[0];
    assert_eq!(hit["path"], "/wiki/mcp-e2e.md");
    assert_eq!(hit["line_no"], 3, "grep reports 1-indexed line");

    // grep clips over-long matched lines (locator, not reader).
    let long_line = format!("LONGLINE_MARK {}", "x".repeat(3000));
    assert_eq!(
        put_file(app.router.clone(), wk, "/wiki/long.md", &long_line).await,
        StatusCode::OK
    );
    let r = call_tool(
        app.router.clone(),
        wk,
        "grep",
        json!({ "pattern": "LONGLINE_MARK" }),
    )
    .await;
    let hits: Value = serde_json::from_str(tool_text(&r)).unwrap();
    let line = hits.as_array().unwrap()[0]["line"].as_str().unwrap();
    assert!(
        line.len() < 600 && line.ends_with('…'),
        "long line must be clipped, got {} bytes",
        line.len()
    );

    // ── read-only wk_: every tool must work (the recommended consumer key) ──
    let ro_wk = {
        let raw = format!("wk_{}", Uuid::new_v4().simple());
        app.state
            .auth_store
            .create_workspace_key(&WorkspaceKey {
                id: Uuid::new_v4().to_string(),
                workspace_id: fs_ws.ws_id.clone(),
                account_id: fs_ws.acct_id.clone(),
                name: "mcp-test-ro".into(),
                key_hash: sha256_hex(raw.as_bytes()),
                permission: KeyPermission::Read,
                status: KeyStatus::Active,
                kind: WorkspaceKind::Fs,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        raw
    };
    for (tool, args) in [
        ("grep", json!({ "pattern": SENTINEL })),
        ("read_file", json!({ "path": "/wiki/mcp-e2e.md" })),
        ("list_dir", json!({ "path": "/wiki" })),
    ] {
        let r = call_tool(app.router.clone(), &ro_wk, tool, args).await;
        assert!(
            !tool_is_error(&r) && r["error"].is_null(),
            "read-only key must run {tool}: {r}"
        );
    }

    // ── degraded modes: overview (summaries off) / ask (no llm) ──
    let r = call_tool(
        app.router.clone(),
        wk,
        "overview",
        json!({ "path": "/wiki/mcp-e2e.md" }),
    )
    .await;
    assert!(tool_is_error(&r));
    assert!(
        tool_text(&r).contains("disabled"),
        "overview disabled text: {}",
        tool_text(&r)
    );
    let r = call_tool(
        app.router.clone(),
        wk,
        "ask",
        json!({ "question": "重试策略是什么?" }),
    )
    .await;
    assert!(tool_is_error(&r));
    assert!(
        tool_text(&r).contains("disabled"),
        "ask disabled text: {}",
        tool_text(&r)
    );

    // ── search: drive the worker until the chunk lands in Milvus ──
    let worker = Worker::new(
        app.mysql.clone(),
        app.mysql.clone(),
        app.milvus.clone(),
        app.mysql.clone(),
        app.milvus.clone(),
        app.embedding.clone(),
        None,
        4,
        1,
        2048,
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    let worker_handle = tokio::spawn(async move { worker.run(stop_rx).await });

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let r = call_tool(
            app.router.clone(),
            wk,
            "search",
            json!({ "query": "定时任务的重试策略", "limit": 5, "detail_level": "full" }),
        )
        .await;
        if !tool_is_error(&r) {
            let hits: Value = serde_json::from_str(tool_text(&r)).unwrap();
            if hits
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["path"] == "/wiki/mcp-e2e.md")
            {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = stop_tx.send(true);
    let _ = worker_handle.await;
    assert!(found, "hybrid search never surfaced the seeded page");

    // search with a path_prefix that excludes the hit
    let r = call_tool(
        app.router.clone(),
        wk,
        "search",
        json!({ "query": "定时任务的重试策略", "path_prefix": "/elsewhere" }),
    )
    .await;
    assert!(!tool_is_error(&r));
    let hits: Value = serde_json::from_str(tool_text(&r)).unwrap();
    assert!(
        hits.as_array().unwrap().is_empty(),
        "path_prefix filter leaked: {hits}"
    );

    cleanup(&app.state, &app.mysql, &[&fs_ws, &db_ws]).await;
}

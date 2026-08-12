//! HTTP roundtrip for `GET /v1/layout` and the MCP `layout` tool, against real
//! MySQL + Milvus + embedding via `tower::ServiceExt::oneshot` (no TCP).
//!
//! Summaries are inserted directly with `upsert_summary` rather than driven
//! through the LLM worker. What needs a real database here is the SQL the
//! layout introduces — the `SUBSTRING_INDEX` GROUP BY that has no serving
//! index, the `ORDER BY is_dir DESC, path LIMIT ?` child listing, and the
//! two batched summary lookups. Summary *generation* is covered by the
//! worker's own tests; wiring an LLM in here would only add flakiness.
//!
//! Three `#[ignore]`d tests, each building its own app because sqlx pools
//! are tied to the runtime that created them: the main endpoint sweep, the
//! case/accent path semantics, and the summaries-disabled behaviour (which
//! needs a differently configured `AppState`).
//!
//! Run with:
//!   NO_PROXY='*' cargo test -p veda-server --test map_test -- --ignored --test-threads=1

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
use veda_core::store::{EmbeddingService, MetadataStore, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, FileSummary, KeyPermission, KeyStatus, SummaryStatus, Workspace,
    WorkspaceKey, WorkspaceKind, WorkspaceStatus,
};

// ── config (same shape as mcp_http_test) ────────────────

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
    router: Router,
}

/// `summary_enabled` is a parameter because the map's `disabled` state is a
/// server-config concern, and the interesting assertion is that a disabled
/// server still serves abstracts it already has.
async fn build_app(summary_enabled: bool) -> TestApp {
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
        EmbeddingProvider::new(
            &cfg.embedding.api_url,
            &cfg.embedding.api_key,
            &cfg.embedding.model,
            Some(cfg.embedding.dimension),
            cfg.embedding.batch_size,
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
        embedding_dim: cfg.embedding.dimension,
        sql_engine,
        metrics: test_metrics(),
        metrics_token: None,
        admin_token: None,
        memory_service: std::sync::Arc::new(veda_core::service::memory::MemoryService::new(
            mysql.clone(),
            milvus.clone(),
            embedding.clone(),
            mysql.clone(),
        )),
        summary_enabled,
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
        router,
    }
}

// ── provisioning ────────────────────────────────────────

struct WsSetup {
    acct_id: String,
    ws_id: String,
    wk: String,
}

async fn provision_workspace(state: &AppState, kind: WorkspaceKind) -> WsSetup {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "map-test".into(),
            email: Some(format!("{}@map-test.com", &acct_id[..8])),
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
            name: "map-ws".into(),
            status: WorkspaceStatus::Active,
            kind,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let wk = format!("wk_{}", Uuid::new_v4().simple());
    state
        .auth_store
        .create_workspace_key(&WorkspaceKey {
            id: Uuid::new_v4().to_string(),
            workspace_id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "map-test-wk".into(),
            key_hash: sha256_hex(wk.as_bytes()),
            permission: KeyPermission::ReadWrite,
            status: KeyStatus::Active,
            kind,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    WsSetup {
        acct_id,
        ws_id,
        wk,
    }
}

/// `hard_delete_workspace` only removes the `veda_workspaces` row — dentries,
/// summaries and *queued outbox events* survive it. Those leftovers are not
/// inert: this suite shares one MySQL with every other integration test, and
/// whichever suite next spins up a worker will spend its deadline draining
/// our backlog instead of its own task. Clean up after ourselves.
async fn cleanup(state: &AppState, mysql: &MysqlStore, setups: &[&WsSetup]) {
    for s in setups {
        let _ = state.auth_store.hard_delete_workspace(&s.ws_id).await;
        // Content tables key off file_id, not workspace_id, so they have to
        // be cleared through the file ids before `veda_files` goes.
        for table in [
            "veda_file_contents",
            "veda_file_blobs",
            "veda_file_extracts",
            "veda_file_chunks",
        ] {
            let _ = sqlx::query(&format!(
                "DELETE FROM {table} WHERE file_id IN \
                 (SELECT id FROM veda_files WHERE workspace_id = ?)"
            ))
            .bind(&s.ws_id)
            .execute(mysql.pool())
            .await;
        }
        for table in [
            "veda_outbox",
            "veda_summaries",
            "veda_dentries",
            "veda_files",
            "veda_fs_events",
            "veda_workspace_keys",
        ] {
            let _ = sqlx::query(&format!("DELETE FROM {table} WHERE workspace_id = ?"))
                .bind(&s.ws_id)
                .execute(mysql.pool())
                .await;
        }
        let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
            .bind(&s.acct_id)
            .execute(mysql.pool())
            .await;
    }
}

// ── request helpers ─────────────────────────────────────

async fn get_layout(router: Router, token: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::builder().method("GET").uri("/v1/layout");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let resp = router.oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn put_file(router: Router, token: &str, path: &str, content: &str) -> StatusCode {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/fs{path}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(content.to_string()))
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

/// Bulk top-level entries are created as directories, not files. A file
/// enqueues ChunkSync + SummarySync, and this suite shares its MySQL with
/// the rest of the integration tests — 250 files would leave ~500 queued
/// embedding jobs behind for whichever suite next starts a worker, which is
/// enough to burn its whole deadline. Directories exercise the cap and the
/// directories-first ordering just as well, for zero downstream work.
async fn mkdir(router: Router, token: &str, path: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/fs-mkdir")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "path": path }).to_string()))
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

async fn mcp_call(router: Router, token: &str, tool: &str) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": {} }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Attach a ready L0 to a path's dentry (directory) or file.
async fn set_abstract(mysql: &MysqlStore, ws_id: &str, path: &str, l0: &str) {
    let d = mysql
        .get_dentry(ws_id, path)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no dentry at {path}"));
    let now = Utc::now();
    let s = FileSummary {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws_id.to_string(),
        file_id: if d.is_dir { None } else { d.file_id.clone() },
        dentry_id: if d.is_dir { Some(d.id.clone()) } else { None },
        l0_abstract: l0.into(),
        l1_overview: String::new(),
        status: SummaryStatus::Ready,
        created_at: now,
        updated_at: now,
    };
    mysql.upsert_summary(&s).await.unwrap();
}

fn entry<'a>(layout: &'a Value, path: &str) -> &'a Value {
    layout["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == path)
        .unwrap_or_else(|| panic!("no entry {path} in {layout}"))
}

// ── the suite ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn layout_endpoint_against_real_mysql() {
    let app = build_app(true).await;
    let fs_ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;
    let db_ws = provision_workspace(&app.state, WorkspaceKind::Db).await;

    // ── auth: no bearer is rejected before any kind check ──
    let (status, _) = get_layout(app.router.clone(), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // ── auth: a db-kind wk_ cannot reach an fs-only endpoint ──
    let (status, body) = get_layout(app.router.clone(), Some(&db_ws.wk)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "WORKSPACE_KIND_MISMATCH");

    // ── an empty workspace still answers ──
    let (status, body) = get_layout(app.router.clone(), Some(&fs_ws.wk)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["entries"].as_array().unwrap().len(), 0);
    assert_eq!(body["data"]["stats"]["total_files"], 0);
    assert_eq!(body["data"]["truncated"], false);

    // ── populate: two areas plus a root-level file ──
    for (path, content) in [
        ("/docs/a.md", "alpha"),
        ("/docs/b.md", "bravo"),
        ("/wiki/c.md", "charlie"),
        ("/README.md", "readme body"),
    ] {
        assert_eq!(
            put_file(app.router.clone(), &fs_ws.wk, path, content).await,
            StatusCode::OK,
            "PUT {path}"
        );
    }

    // No summaries yet, but the layout is already useful.
    let (status, body) = get_layout(app.router.clone(), Some(&fs_ws.wk)).await;
    assert_eq!(status, StatusCode::OK);
    let layout = &body["data"];
    assert_eq!(
        layout["summary_state"], "partial",
        "summaries pending must not be 202/501: {layout}"
    );

    // Directories first, then files — the truncation order.
    let paths: Vec<&str> = layout["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["/docs", "/wiki", "/README.md"]);

    // The SUBSTRING_INDEX GROUP BY: counts are per top-level area and
    // recursive, and a root-level file must not pick one up even though it
    // groups under its own name in that query.
    assert_eq!(entry(layout, "/docs")["file_count"], 2);
    assert_eq!(entry(layout, "/wiki")["file_count"], 1);
    assert!(
        entry(layout, "/README.md").get("file_count").is_none(),
        "a file must not report file_count"
    );
    assert_eq!(entry(layout, "/README.md")["size_bytes"], 11);
    assert!(entry(layout, "/docs").get("size_bytes").is_none());

    assert_eq!(layout["stats"]["total_files"], 4);
    assert_eq!(layout["stats"]["total_directories"], 2);

    // ── with every abstract present the state flips to ready ──
    set_abstract(&app.mysql, &fs_ws.ws_id, "/docs", "project documentation").await;
    set_abstract(&app.mysql, &fs_ws.ws_id, "/wiki", "team wiki").await;
    set_abstract(&app.mysql, &fs_ws.ws_id, "/README.md", "the readme").await;
    let (_, body) = get_layout(app.router.clone(), Some(&fs_ws.wk)).await;
    let layout = &body["data"];
    assert_eq!(layout["summary_state"], "ready", "{layout}");
    assert_eq!(entry(layout, "/docs")["abstract"], "project documentation");
    assert_eq!(entry(layout, "/README.md")["abstract"], "the readme");

    // ── MCP returns the same payload as REST's `data` ──
    let rpc = mcp_call(app.router.clone(), &fs_ws.wk, "layout").await;
    assert_eq!(rpc["result"]["isError"], false, "{rpc}");
    let text = rpc["result"]["content"][0]["text"].as_str().unwrap();
    let via_mcp: Value = serde_json::from_str(text).unwrap();
    assert_eq!(&via_mcp, layout, "MCP and REST must agree");

    // ── truncation is real, and reading it does not blow up ──
    for i in 0..250 {
        assert_eq!(
            mkdir(app.router.clone(), &fs_ws.wk, &format!("/bulk{i:03}")).await,
            StatusCode::OK
        );
    }
    let (status, body) = get_layout(app.router.clone(), Some(&fs_ws.wk)).await;
    assert_eq!(status, StatusCode::OK);
    let layout = &body["data"];
    assert_eq!(layout["entries"].as_array().unwrap().len(), 200);
    assert_eq!(layout["truncated"], true);
    // Ordering is directories-then-path, so the cap keeps the first 200
    // directory names and /README.md — a file — falls off the end.
    assert_eq!(layout["entries"][0]["path"], "/bulk000");
    assert!(
        layout["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["is_dir"] == true),
        "files must be the first thing truncation drops"
    );
    // stats still describe the whole workspace, not the truncated page.
    assert_eq!(layout["stats"]["total_files"], 4);
    assert_eq!(layout["stats"]["total_directories"], 252);

    cleanup(&app.state, &app.mysql, &[&fs_ws, &db_ws]).await;
}

/// Path comparison in veda is case- and accent-insensitive, because
/// `veda_dentries.path` is `utf8mb4_0900_ai_ci` and both `get_dentry` and
/// `list_dentries` compare against it directly. So `/Docs` and `/docs` are
/// ONE directory — the second spelling never gets its own dentry — and a
/// listing of it returns files written under either. `file_count` has to
/// agree with that: it must report the union, not a per-spelling split, or
/// the layout contradicts the `list_dir` of the very directory it describes.
///
/// (This also pins the Rust-side lookup. MySQL returns one arbitrary
/// spelling per group, so matching it against the directory's `name`
/// requires folding both — otherwise a `Docs` directory whose group came
/// back keyed `docs` silently reports zero.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn file_counts_follow_the_case_insensitive_path_semantics() {
    let app = build_app(true).await;
    let ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;

    for path in [
        "/Docs/upper.md",
        "/docs/lower-a.md",
        "/docs/lower-b.md",
        "/café/accented.md",
        "/cafe/plain-a.md",
    ] {
        assert_eq!(
            put_file(app.router.clone(), &ws.wk, path, "x").await,
            StatusCode::OK,
            "PUT {path}"
        );
    }

    let (status, body) = get_layout(app.router.clone(), Some(&ws.wk)).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let layout = &body["data"];

    // Only the first spelling of each becomes a directory dentry.
    assert_eq!(layout["stats"]["total_directories"], 2, "{layout}");
    assert_eq!(layout["stats"]["total_files"], 5, "{layout}");
    assert_eq!(layout["entries"].as_array().unwrap().len(), 2, "{layout}");

    // ...and it counts every file underneath, whichever spelling wrote it.
    assert_eq!(entry(layout, "/Docs")["file_count"], 3, "{layout}");
    assert_eq!(entry(layout, "/café")["file_count"], 2, "{layout}");

    cleanup(&app.state, &app.mysql, &[&ws]).await;
}

/// A server with no `[llm]` reports `disabled` — but must still hand back
/// abstracts already in the database, because `/v1/abstract/{path}` does.
/// Hiding them here would make the two endpoints disagree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn disabled_summaries_still_return_cached_abstracts() {
    let app = build_app(false).await;
    let ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;

    assert_eq!(
        put_file(app.router.clone(), &ws.wk, "/docs/a.md", "alpha").await,
        StatusCode::OK
    );
    set_abstract(&app.mysql, &ws.ws_id, "/docs", "cached summary").await;

    let (status, body) = get_layout(app.router.clone(), Some(&ws.wk)).await;
    assert_eq!(status, StatusCode::OK);
    let layout = &body["data"];
    assert_eq!(layout["summary_state"], "disabled");
    assert_eq!(
        entry(layout, "/docs")["abstract"], "cached summary",
        "disabled must not hide an abstract /v1/abstract would serve: {layout}"
    );

    cleanup(&app.state, &app.mysql, &[&ws]).await;
}


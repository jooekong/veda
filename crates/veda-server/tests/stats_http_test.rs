//! Doc access heat stats against real MySQL: the counting read surfaces
//! (`GET /v1/fs`), the scan exemption (`POST /v1/grep`), flush → upsert →
//! `GET /v1/stats/docs` aggregation, rename continuity, delete drop-off,
//! and the auth/kind gates.
//!
//! `search_hits` enters through `record_search_hits` directly rather than a
//! worker-indexed live search: the search()→dedup→record link is pinned by
//! unit tests (search_test.rs), real hybrid retrieval is pinned by
//! mcp_http_test — what needs a real database HERE is the shared
//! upsert/aggregate SQL, which reads and hits exercise identically.
//! Driving the embedding worker would only add flakiness.
//!
//! Run with:
//!   NO_PROXY='*' cargo test -p veda-server --test stats_http_test -- --ignored --test-threads=1

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
use veda_core::service::access_stats::AccessRecorder;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
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
    recorder: Arc<AccessRecorder>,
}

/// Unlike the other suites, fs/search services here are wired with an
/// ENABLED recorder — that's the subject under test.
async fn build_app() -> TestApp {
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

    let recorder = Arc::new(AccessRecorder::new(mysql.clone(), 8, true));
    let fs_service = Arc::new(FsService::with_stats(mysql.clone(), recorder.clone()));
    let search_service = SearchService::with_stats(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        recorder.clone(),
    );
    let collection_service =
        CollectionService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let sql_engine = veda_sql::VedaSqlEngine::new(
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        // Production wiring: the SQL engine reads through an UNCOUNTED
        // FsService (scan-surface exemption).
        Arc::new(FsService::new(mysql.clone())),
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
        summary_enabled: false,
        answer_service: None,
        answer_concurrency: 2,
        tunnel_bots: Arc::new(
            veda_server::tunnel_bots::TunnelBotStore::connect(&cfg.mysql.database_url)
                .await
                .expect("tunnel bots store"),
        ),
        access_recorder: recorder.clone(),
        draining: std::sync::atomic::AtomicBool::new(false),
    });
    let router = build_router(state.clone());
    TestApp {
        state,
        mysql,
        router,
        recorder,
    }
}

struct WsSetup {
    acct_id: String,
    ws_id: String,
    wk: String,
    wk_readonly: String,
}

async fn provision_workspace(state: &AppState, kind: WorkspaceKind) -> WsSetup {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "stats-test".into(),
            email: Some(format!("{}@stats-test.com", &acct_id[..8])),
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
            name: "stats-ws".into(),
            status: WorkspaceStatus::Active,
            kind,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let mut keys = Vec::new();
    for perm in [KeyPermission::ReadWrite, KeyPermission::Read] {
        let wk = format!("wk_{}", Uuid::new_v4().simple());
        state
            .auth_store
            .create_workspace_key(&WorkspaceKey {
                id: Uuid::new_v4().to_string(),
                workspace_id: ws_id.clone(),
                account_id: acct_id.clone(),
                name: format!("stats-test-{perm:?}"),
                key_hash: sha256_hex(wk.as_bytes()),
                permission: perm,
                status: KeyStatus::Active,
                kind,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        keys.push(wk);
    }
    let wk_readonly = keys.pop().unwrap();
    let wk = keys.pop().unwrap();
    WsSetup {
        acct_id,
        ws_id,
        wk,
        wk_readonly,
    }
}

async fn cleanup(state: &AppState, mysql: &MysqlStore, setups: &[&WsSetup]) {
    for s in setups {
        let _ = state.auth_store.hard_delete_workspace(&s.ws_id).await;
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
            "veda_doc_access_daily",
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

async fn req(
    router: Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = router.oneshot(b.body(body).unwrap()).await.unwrap();
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

fn items(body: &Value) -> Vec<(String, u64, u64)> {
    body["data"]["items"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|i| {
                    (
                        i["path"].as_str().unwrap_or("").to_string(),
                        i["search_hits"].as_u64().unwrap_or(0),
                        i["reads"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── tests ───────────────────────────────────────────────

// multi_thread is REQUIRED: the SQL exemption assertion drives `veda_read()`,
// whose fs_udf::block_on parks a scoped thread on Handle::block_on — under
// the default current_thread runtime the only driver thread is blocked in
// pthread_join waiting for that scoped thread, a guaranteed deadlock.
// Production always runs multi-thread, so this is a test-harness concern only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn stats_end_to_end_counts_rename_delete() {
    let app = build_app().await;
    let ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;

    // Two text files; each PUT enqueues outbox work we clean up at the end.
    assert_eq!(
        put_file(app.router.clone(), &ws.wk, "/hot.md", "needle in hot file").await,
        StatusCode::OK
    );
    assert_eq!(
        put_file(app.router.clone(), &ws.wk, "/cold.md", "needle in cold file").await,
        StatusCode::OK
    );

    // hot: 2 reads. cold: 1 read.
    for _ in 0..2 {
        let (st, _) = req(app.router.clone(), "GET", "/v1/fs/hot.md", Some(&ws.wk), None).await;
        assert_eq!(st, StatusCode::OK);
    }
    let (st, _) = req(app.router.clone(), "GET", "/v1/fs/cold.md", Some(&ws.wk), None).await;
    assert_eq!(st, StatusCode::OK);

    // Scan exemption: grep sweeps both files but must not move `reads`.
    let (st, grep_body) = req(
        app.router.clone(),
        "POST",
        "/v1/grep",
        Some(&ws.wk),
        Some(json!({"pattern": "needle"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(grep_body["data"].as_array().map(|a| a.len()), Some(2));

    // Scan exemption #2: SQL reads go through the uncounted engine instance
    // (production wiring assembled above) — must not move `reads` either.
    let (st, _) = req(
        app.router.clone(),
        "POST",
        "/v1/sql",
        Some(&ws.wk),
        Some(json!({"sql": "SELECT veda_read('/hot.md') AS c"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // search_hits via the recorder (see module doc for why not a live search).
    let hot_dentry: (String,) =
        sqlx::query_as("SELECT id FROM veda_dentries WHERE workspace_id = ? AND path = '/hot.md'")
            .bind(&ws.ws_id)
            .fetch_one(app.mysql.pool())
            .await
            .unwrap();
    app.recorder.record_search_hits(&ws.ws_id, &[hot_dentry.0]);

    // Flush drains; second flush is a no-op (no double-count).
    assert!(app.recorder.flush().await.unwrap() >= 2);
    assert_eq!(app.recorder.flush().await.unwrap(), 0);

    let (st, body) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?days=1",
        Some(&ws.wk),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let got = items(&body);
    assert_eq!(
        got,
        vec![
            ("/hot.md".to_string(), 1, 2),
            ("/cold.md".to_string(), 0, 1)
        ],
        "default order is reads DESC; grep must not have inflated either row"
    );

    // order_by=search_hits flips the board.
    let (_, body) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?days=1&order_by=search_hits",
        Some(&ws.wk),
        None,
    )
    .await;
    assert_eq!(items(&body)[0].0, "/hot.md");

    // Additive upsert: a second flush cycle accumulates onto the same
    // (workspace, day, dentry) row instead of replacing it.
    let (st, _) = req(app.router.clone(), "GET", "/v1/fs/hot.md", Some(&ws.wk), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(app.recorder.flush().await.unwrap(), 1);
    let (_, body) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?days=1",
        Some(&ws.wk),
        None,
    )
    .await;
    assert!(
        items(&body).iter().any(|(p, _, r)| p == "/hot.md" && *r == 3),
        "second flush must add, not replace: {:?}",
        items(&body)
    );

    // Rename keeps history (dentry_id survives, path column is live).
    let (st, _) = req(
        app.router.clone(),
        "POST",
        "/v1/fs-rename",
        Some(&ws.wk),
        Some(json!({"from": "/cold.md", "to": "/renamed.md"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, body) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?days=1",
        Some(&ws.wk),
        None,
    )
    .await;
    let got = items(&body);
    assert!(
        got.iter().any(|(p, _, r)| p == "/renamed.md" && *r == 1),
        "renamed doc keeps its counts under the new path: {got:?}"
    );

    // Delete drops the doc off the board (inner join on live dentries).
    let (st, _) = req(
        app.router.clone(),
        "DELETE",
        "/v1/fs/renamed.md",
        Some(&ws.wk),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, body) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?days=1",
        Some(&ws.wk),
        None,
    )
    .await;
    let got = items(&body);
    assert!(
        got.iter().all(|(p, _, _)| p != "/renamed.md"),
        "deleted doc must vanish from the board: {got:?}"
    );

    cleanup(&app.state, &app.mysql, &[&ws]).await;
}

// multi_thread is REQUIRED: the SQL exemption assertion drives `veda_read()`,
// whose fs_udf::block_on parks a scoped thread on Handle::block_on — under
// the default current_thread runtime the only driver thread is blocked in
// pthread_join waiting for that scoped thread, a guaranteed deadlock.
// Production always runs multi-thread, so this is a test-harness concern only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn stats_auth_kind_and_validation_gates() {
    let app = build_app().await;
    let fs_ws = provision_workspace(&app.state, WorkspaceKind::Fs).await;
    let db_ws = provision_workspace(&app.state, WorkspaceKind::Db).await;

    // No token → 401.
    let (st, _) = req(app.router.clone(), "GET", "/v1/stats/docs", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Read-only wk_ may query (stats are read-only info).
    let (st, _) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs",
        Some(&fs_ws.wk_readonly),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // db-kind key → workspace_kind_mismatch 400 (enforced by AuthWorkspace).
    let (st, _) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs",
        Some(&db_ws.wk),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Bogus order_by → 400, not a silent default.
    let (st, _) = req(
        app.router.clone(),
        "GET",
        "/v1/stats/docs?order_by=likes",
        Some(&fs_ws.wk),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    cleanup(&app.state, &app.mysql, &[&fs_ws, &db_ws]).await;
}

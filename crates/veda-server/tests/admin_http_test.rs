//! HTTP roundtrip for the read-only admin surface (`/admin/v1/*`).
//!
//! Runs the real `build_router(AppState)` against real MySQL + Milvus +
//! embedding, dispatching via `tower::ServiceExt::oneshot` (no TCP). One
//! `#[ignore]`d mega-test (sqlx pools are tied to the runtime that created
//! them, so all sub-checks share one runtime) covers:
//!   - fail-closed: admin surface 404s when `admin_token` is unset
//!   - auth: missing / wrong bearer → 401
//!   - list: cross-tenant workspace list with dataset/key counts + fs stats
//!   - detail: per-dataset live Milvus `count(*)` equals upserted rows
//!   - vector console: admin search returns hits
//!   - documents: fs file listing
//!
//! Run with: `NO_PROXY='*' cargo test -p veda-server --test admin_http_test -- --ignored --test-threads=1`

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
use veda_core::store::{EmbeddingService, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, Dataset, DatasetStatus, KeyPermission, KeyStatus, Workspace,
    WorkspaceKey, WorkspaceKind, WorkspaceStatus,
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

/// Build a real app with `admin_token` set to the given value (None = admin
/// surface disabled, for the fail-closed check).
async fn build_admin_app(
    admin_token: Option<String>,
) -> (Arc<AppState>, Arc<MysqlStore>, Router) {
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
        admin_token,
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
    (state, mysql, router)
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
    let request = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Raw-body request (for fs writes — body is the file content, not JSON).
async fn req_raw(router: Router, method: &str, uri: &str, token: &str, body: &str) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    router.oneshot(request).await.unwrap().status()
}

// ── provisioning ────────────────────────────────────────
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
            name: "admin-test".into(),
            email: Some(format!("{}@admin-test.com", &acct_id[..8])),
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
            name: "admin-test-wk".into(),
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

async fn provision_db_workspace(state: &AppState) -> WsSetup {
    let acct_id = create_account(state).await;
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "admin-db".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Db,
            app_id: Some("admin-test-tenant".into()),
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    state
        .auth_store
        .create_dataset(&Dataset {
            id: Uuid::new_v4().to_string(),
            workspace_id: ws_id.clone(),
            name: "default".into(),
            status: DatasetStatus::Active,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    state
        .vector_workspace_store
        .create_vector_collection(&ws_id, state.embedding_dim)
        .await
        .unwrap();
    let wk = create_wk(state, &acct_id, &ws_id, WorkspaceKind::Db).await;
    WsSetup {
        acct_id,
        ws_id,
        wk,
    }
}

async fn provision_fs_workspace(state: &AppState) -> WsSetup {
    let acct_id = create_account(state).await;
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "admin-fs".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Fs,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let wk = create_wk(state, &acct_id, &ws_id, WorkspaceKind::Fs).await;
    WsSetup {
        acct_id,
        ws_id,
        wk,
    }
}

async fn cleanup(state: &AppState, mysql: &MysqlStore, setups: &[&WsSetup]) {
    for s in setups {
        let _ = state
            .vector_workspace_store
            .drop_collection(&veda_store::vector_collection_name(&s.ws_id))
            .await;
        let _ = state
            .auth_store
            .hard_delete_datasets_for_workspace(&s.ws_id)
            .await;
        let _ = state.auth_store.hard_delete_workspace(&s.ws_id).await;
        let _ = sqlx::query("DELETE FROM veda_workspace_keys WHERE workspace_id = ?")
            .bind(&s.ws_id)
            .execute(mysql.pool())
            .await;
        let _ = sqlx::query("DELETE FROM veda_doc_access_daily WHERE workspace_id = ?")
            .bind(&s.ws_id)
            .execute(mysql.pool())
            .await;
        let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
            .bind(&s.acct_id)
            .execute(mysql.pool())
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // needs real MySQL/Milvus/embedding — run explicitly with `--ignored`
async fn admin_http_suite() {
    const ADMIN: &str = "test-admin-secret";
    let (state, mysql, router) = build_admin_app(Some(ADMIN.into())).await;

    // ── fail-closed: a separate app with admin disabled 404s the surface ──
    {
        let (_s, _m, router_off) = build_admin_app(None).await;
        let (st, _) = req(router_off, "GET", "/admin/v1/workspaces", Some(ADMIN), None).await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "disabled admin surface must 404, not leak"
        );
    }

    // ── auth: missing / wrong bearer → 401 ──
    let (st, _) = req(router.clone(), "GET", "/admin/v1/workspaces", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no bearer → 401");
    let (st, _) = req(router.clone(), "GET", "/admin/v1/workspaces", Some("wrong"), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "wrong bearer → 401");

    // ── provision a db + fs workspace ──
    let db = provision_db_workspace(&state).await;
    let fs = provision_fs_workspace(&state).await;

    // Upsert 3 vectors through the data plane (wk_).
    let up = json!({"records":[
        {"text":"the quick brown fox"},
        {"text":"lorem ipsum dolor sit"},
        {"text":"vector databases are fun"}
    ]});
    let (st, b) = req(
        router.clone(),
        "POST",
        "/v1/vectors/upsert",
        Some(&db.wk),
        Some(up),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "vectors upsert failed: {b}");

    // Write a file through the fs data plane (wk_).
    let st = req_raw(router.clone(), "PUT", "/v1/fs/hello.txt", &fs.wk, "hello veda admin").await;
    assert!(st.is_success(), "fs write failed: {st}");

    // ── list: both workspaces present with correct counts/stats ──
    let (st, body) = req(router.clone(), "GET", "/admin/v1/workspaces", Some(ADMIN), None).await;
    assert_eq!(st, StatusCode::OK);
    let items = body["data"].as_array().expect("data array");
    let db_row = items
        .iter()
        .find(|w| w["id"] == db.ws_id)
        .expect("db ws in admin list");
    assert_eq!(db_row["kind"], "db");
    assert_eq!(db_row["app_id"], "admin-test-tenant");
    assert_eq!(db_row["dataset_count"], 1, "db dataset_count");
    assert_eq!(db_row["key_count"], 1, "db key_count");
    let fs_row = items
        .iter()
        .find(|w| w["id"] == fs.ws_id)
        .expect("fs ws in admin list");
    assert_eq!(fs_row["kind"], "fs");
    assert_eq!(fs_row["files"]["total_files"], 1, "fs total_files");

    // ── detail: per-dataset live Milvus count(*) == upserted rows ──
    let (st, body) = req(
        router.clone(),
        "GET",
        &format!("/admin/v1/workspaces/{}", db.ws_id),
        Some(ADMIN),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let datasets = body["data"]["datasets"].as_array().expect("datasets array");
    let default_ds = datasets
        .iter()
        .find(|d| d["name"] == "default")
        .expect("default dataset");
    assert_eq!(
        default_ds["vector_count"], 3,
        "Milvus count(*) must equal the 3 upserted rows"
    );

    // ── vector console: admin search returns hits ──
    let search = json!({"query":"quick fox","top_k":5,"mode":"hybrid"});
    let (st, body) = req(
        router.clone(),
        "POST",
        &format!("/admin/v1/workspaces/{}/vectors/search", db.ws_id),
        Some(ADMIN),
        Some(search),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "admin search failed: {body}");
    assert!(
        !body["data"].as_array().unwrap().is_empty(),
        "admin search returned no hits"
    );

    // ── admin upsert (the mutating console) + category/tag filter (array_contains) ──
    let up = json!({"text":"category tag filter probe pineapple","category":"fruit","tags":["yellow","sweet"]});
    let (st, body) = req(
        router.clone(),
        "POST",
        &format!("/admin/v1/workspaces/{}/vectors/upsert", db.ws_id),
        Some(ADMIN),
        Some(up),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "admin upsert failed: {body}");
    let new_id = body["data"]["id"]
        .as_str()
        .expect("upsert returns id")
        .to_string();

    let search_uri = format!("/admin/v1/workspaces/{}/vectors/search", db.ws_id);
    let has_new = |body: &serde_json::Value| -> bool {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == new_id)
    };

    // category=fruit → the upserted row is included
    let (st, body) = req(
        router.clone(),
        "POST",
        &search_uri,
        Some(ADMIN),
        Some(json!({"query":"pineapple","category":"fruit","top_k":20})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "category search failed: {body}");
    assert!(has_new(&body), "category=fruit must match the upserted row");

    // category=<other> → the row is excluded (category filter actually filters)
    let (st, body) = req(
        router.clone(),
        "POST",
        &search_uri,
        Some(ADMIN),
        Some(json!({"query":"pineapple","category":"vegetable","top_k":20})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!has_new(&body), "a different category must exclude the row");

    // tag=yellow (array_contains) → matches
    let (st, body) = req(
        router.clone(),
        "POST",
        &search_uri,
        Some(ADMIN),
        Some(json!({"query":"pineapple","tags":["yellow"],"top_k":20})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "tag search failed: {body}");
    assert!(has_new(&body), "tag=yellow (array_contains) must match the row");

    // absent tag → the row is excluded (filter actually filters)
    let (st, body) = req(
        router.clone(),
        "POST",
        &search_uri,
        Some(ADMIN),
        Some(json!({"query":"pineapple","tags":["zzz_absent_tag"],"top_k":20})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!has_new(&body), "an absent tag must exclude the row");

    // ── documents: fs file listing ──
    let (st, body) = req(
        router.clone(),
        "GET",
        &format!("/admin/v1/workspaces/{}/files?path=/", fs.ws_id),
        Some(ADMIN),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let entries = body["data"].as_array().expect("files array");
    assert!(
        entries.iter().any(|e| e["name"] == "hello.txt"),
        "hello.txt not listed in admin files"
    );

    // ── doc heat board (stats/docs) ──
    // The recorder in this app is disabled, so seed the table directly —
    // what's under test HERE is the admin endpoint's auth/kind/query
    // wiring, not the counting pipeline (stats_http_test owns that).
    {
        let dentry: (String,) = sqlx::query_as(
            "SELECT id FROM veda_dentries WHERE workspace_id = ? AND path = '/hello.txt'",
        )
        .bind(&fs.ws_id)
        .fetch_one(mysql.pool())
        .await
        .expect("hello.txt dentry");
        state
            .meta_store
            .upsert_doc_access_daily(&[veda_core::store::DocAccessRow {
                workspace_id: fs.ws_id.clone(),
                day: chrono::Utc::now().date_naive(),
                dentry_id: dentry.0,
                search_hits: 3,
                reads: 7,
            }])
            .await
            .expect("seed doc access row");

        let uri = format!("/admin/v1/workspaces/{}/stats/docs?days=2", fs.ws_id);
        let (st, body) = req(router.clone(), "GET", &uri, Some(ADMIN), None).await;
        assert_eq!(st, StatusCode::OK, "admin stats: {body}");
        let items = body["data"]["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["path"], "/hello.txt");
        assert_eq!(items[0]["search_hits"], 3);
        assert_eq!(items[0]["reads"], 7);

        // db workspace → empty board, not an error.
        let uri = format!("/admin/v1/workspaces/{}/stats/docs", db.ws_id);
        let (st, body) = req(router.clone(), "GET", &uri, Some(ADMIN), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["data"]["items"].as_array().map(|a| a.len()), Some(0));

        // unknown workspace → 404; no bearer → 401.
        let (st, _) = req(
            router.clone(),
            "GET",
            "/admin/v1/workspaces/nonexistent/stats/docs",
            Some(ADMIN),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = req(
            router.clone(),
            "GET",
            &format!("/admin/v1/workspaces/{}/stats/docs", fs.ws_id),
            None,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }

    cleanup(&state, &mysql, &[&db, &fs]).await;
}

//! Platform-gateway data plane (`/v1/workspace/{app_id}/project/{id}/...`).
//!
//! Runs the real `build_router(AppState)` against real MySQL + Milvus +
//! embedding, dispatching via `tower::ServiceExt::oneshot`. Verifies that the
//! platform surface (project_data.rs) wraps the data plane correctly:
//!   - db vectors upsert/search/query/delete resolve project by path
//!   - fs query (files/search) resolves project by path
//!   - kind mismatch (db path on fs project) → WORKSPACE_KIND_MISMATCH
//!   - response shapes match the company envelope: list/search → paged
//!     `{data:[...],page,...}`; single回执 (upsert/delete) → bare object;
//!     error → `{error:{code,...}}`
//!
//! No `VEDA_PLATFORM_BASE` is set, so external authz is skipped and the
//! `GatewayUser` extractor yields an empty identity — same as management-plane
//! dev behavior. Setup goes through direct store calls.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, Dataset, DatasetStatus, Workspace, WorkspaceKind, WorkspaceStatus,
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

async fn build_test_app() -> (Arc<AppState>, axum::Router) {
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
    (state, router)
}

struct Setup {
    /// account.app_id — the `{workspace}` path segment.
    app_id: String,
    db_ws_id: String,
    fs_ws_id: String,
}

/// One account (with app_id) owning one db project + one fs project.
async fn provision(state: &AppState) -> Setup {
    let acct_id = Uuid::new_v4().to_string();
    let app_id = format!("app-{}", Uuid::new_v4().simple());
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "proj-data-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            app_id: Some(app_id.clone()),
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    // db project: workspace + default dataset + Milvus collection.
    let db_ws_id = Uuid::new_v4().to_string();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: db_ws_id.clone(),
            account_id: acct_id.clone(),
            name: "db-proj".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Db,
            app_id: Some(app_id.clone()),
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
            workspace_id: db_ws_id.clone(),
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
        .create_vector_collection(&db_ws_id, state.embedding_dim)
        .await
        .unwrap();

    // fs project: just the workspace (dentry tree starts empty).
    let fs_ws_id = Uuid::new_v4().to_string();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: fs_ws_id.clone(),
            account_id: acct_id.clone(),
            name: "fs-proj".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Fs,
            app_id: Some(app_id.clone()),
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    Setup {
        app_id,
        db_ws_id,
        fs_ws_id,
    }
}

async fn post(router: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(router, req).await
}

async fn get(router: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    send(router, req).await
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
#[ignore = "needs real MySQL + Milvus + embedding (config/test.toml); run with --ignored"]
async fn platform_data_plane_roundtrip() {
    let (state, router) = build_test_app().await;
    let s = provision(&state).await;
    let db = |p: &str| format!("/v1/workspace/{}/project/{}{}", s.app_id, s.db_ws_id, p);
    let fs = |p: &str| format!("/v1/workspace/{}/project/{}{}", s.app_id, s.fs_ws_id, p);

    // ── db upsert → bare object {ids, commit_ts} (single回执, NOT a page) ──
    let (st, body) = post(
        &router,
        &db("/vectors/upsert"),
        json!({ "records": [
            { "id": "sku-1", "text": "红色棉质圆领 T 恤", "category": "服装", "tags": ["红色"] }
        ]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "upsert: {body}");
    assert_eq!(body["ids"][0], "sku-1", "upsert returns bare ids: {body}");
    assert!(body["commit_ts"].is_number(), "bare commit_ts: {body}");
    assert!(
        body.get("data").is_none() && body.get("page").is_none(),
        "upsert回执 is a bare object, not a page envelope: {body}"
    );

    // ── db search → paged envelope {data:[...], page, total, ...} ──
    let (st, body) = post(
        &router,
        &db("/vectors/search"),
        json!({ "query": "红色 T 恤", "top_k": 5 }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "search: {body}");
    assert!(body["data"].is_array(), "search data is array: {body}");
    assert_eq!(body["data"][0]["id"], "sku-1", "search hit: {body}");
    assert_eq!(body["page"], 1, "paged envelope: {body}");
    assert!(body["total"].is_number(), "paged total: {body}");
    assert!(body["has_next_page"].is_boolean(), "paged flags: {body}");

    // ── db query by id → paged envelope, no score ──
    let (st, body) = post(&router, &db("/vectors/query"), json!({ "ids": ["sku-1"] })).await;
    assert_eq!(st, StatusCode::OK, "query: {body}");
    assert_eq!(body["data"][0]["id"], "sku-1", "query hit: {body}");
    assert!(body["data"][0].get("score").is_none(), "query has no score: {body}");

    // ── fs files (empty dir) → paged envelope with data:[] ──
    let (st, body) = get(&router, &fs("/files?path=/")).await;
    assert_eq!(st, StatusCode::OK, "files: {body}");
    assert!(body["data"].is_array(), "files data is array: {body}");
    assert_eq!(body["page"], 1, "files paged envelope: {body}");

    // ── kind mismatch: db path on the fs project → error envelope ──
    let (st, body) = post(&router, &fs("/vectors/search"), json!({ "query": "x" })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "kind mismatch status: {body}");
    assert_eq!(
        body["error"]["code"], "WORKSPACE_KIND_MISMATCH",
        "error envelope with code: {body}"
    );

    // ── unknown project id → NOT_FOUND error envelope ──
    let (st, body) = post(
        &router,
        &format!(
            "/v1/workspace/{}/project/{}/vectors/search",
            s.app_id,
            Uuid::new_v4()
        ),
        json!({ "query": "x" }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "unknown project: {body}");
    assert_eq!(body["error"]["code"], "NOT_FOUND", "not found envelope: {body}");

    // ── db delete → bare object {delete_count} ──
    let (st, body) = post(&router, &db("/vectors/delete"), json!({ "ids": ["sku-1"] })).await;
    assert_eq!(st, StatusCode::OK, "delete: {body}");
    assert_eq!(body["delete_count"], 1, "bare delete_count: {body}");
    assert!(body.get("data").is_none(), "delete回执 is bare: {body}");
}

/// Raw-byte GET (no JSON parse) for the download endpoint.
async fn get_raw(router: &axum::Router, path: &str) -> (StatusCode, Vec<u8>, String, String) {
    let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let dispo = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, bytes, ctype, dispo)
}

async fn put_bytes(router: &axum::Router, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/octet-stream")
        .body(Body::from(body))
        .unwrap();
    send(router, req).await
}

#[tokio::test]
#[ignore = "needs real MySQL + Milvus + embedding (config/test.toml); run with --ignored"]
async fn platform_fs_upload_download() {
    let (state, router) = build_test_app().await;
    let s = provision(&state).await;
    let fs = |p: &str| format!("/v1/workspace/{}/project/{}{}", s.app_id, s.fs_ws_id, p);
    let db = |p: &str| format!("/v1/workspace/{}/project/{}{}", s.app_id, s.db_ws_id, p);

    // ── text upload → bare WriteFileResponse; parents auto-created ──
    let text = "# 平台上传\n\n中文内容 roundtrip 测试。\n";
    let (st, body) = put_bytes(&router, &fs("/file?path=/docs/说明.md"), text.as_bytes().to_vec()).await;
    assert_eq!(st, StatusCode::OK, "text upload: {body}");
    assert_eq!(body["revision"], 1, "first write revision: {body}");
    assert!(body["file_id"].is_string(), "bare object with file_id: {body}");

    // ── download round-trips the exact bytes with attachment headers ──
    let (st, bytes, ctype, dispo) = get_raw(&router, &fs("/file/content?path=/docs/%E8%AF%B4%E6%98%8E.md")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bytes, text.as_bytes(), "downloaded bytes identical");
    assert!(ctype.contains("markdown") || ctype.contains("text"), "mime: {ctype}");
    assert!(dispo.contains("attachment") && dispo.contains("filename*=UTF-8''"), "disposition: {dispo}");

    // ── overwrite bumps revision (last-write-wins, no preconditions) ──
    let (st, body) = put_bytes(&router, &fs("/file?path=/docs/说明.md"), b"v2".to_vec()).await;
    assert_eq!(st, StatusCode::OK, "overwrite: {body}");
    assert_eq!(body["revision"], 2, "overwrite bumps revision: {body}");

    // ── binary upload (invalid UTF-8) → blob path; byte-exact download ──
    let png: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF, 0x00, 0xFE];
    let (st, body) = put_bytes(&router, &fs("/file?path=/img/logo.png"), png.clone()).await;
    assert_eq!(st, StatusCode::OK, "binary upload: {body}");
    let (st, bytes, ctype, _) = get_raw(&router, &fs("/file/content?path=/img/logo.png")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bytes, png, "binary bytes identical");
    assert!(ctype.contains("png") || ctype.contains("octet"), "binary mime: {ctype}");

    // ── uploaded file shows up in the existing listing/preview surface ──
    let (st, body) = get(&router, &fs("/files?path=/docs")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body["data"].as_array().unwrap().iter().any(|e| e["name"] == "说明.md"),
        "uploaded file listed: {body}"
    );

    // ── kind gate: upload to a db project → WORKSPACE_KIND_MISMATCH ──
    let (st, body) = put_bytes(&router, &db("/file?path=/x.md"), b"x".to_vec()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "db upload: {body}");
    assert_eq!(body["error"]["code"], "WORKSPACE_KIND_MISMATCH");

    // ── download of a missing path → NOT_FOUND error envelope ──
    let (st, bytes, _, _) = get_raw(&router, &fs("/file/content?path=/nope.md")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "missing file: {}", String::from_utf8_lossy(&bytes));
}

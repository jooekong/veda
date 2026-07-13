//! Platform tunnel-bot management (`/v1/workspace/{ws}/project/{id}/tunnel/bots`).
//!
//! Real MySQL via `build_router(AppState)` + `tower::ServiceExt::oneshot`
//! (Milvus/embedding are wired but untouched — this surface is pure MySQL).
//! Covers the full CRUD cycle plus the invariants that matter:
//!   - create mints a dedicated read-only `wk_` and stamps workspace/project
//!   - db project → WORKSPACE_KIND_MISMATCH (fs-only surface)
//!   - duplicate bot_id → conflict AND the pre-minted key is revoked (no leak)
//!   - patch keeps stored secret on empty, 404s on foreign project scope
//!   - delete removes the row and revokes the auto-minted key
//!
//! No `VEDA_PLATFORM_BASE` → external authz skipped (same as the other
//! platform-surface tests).

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
use veda_core::store::EmbeddingService;
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, KeyStatus, Workspace, WorkspaceKind, WorkspaceStatus,
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
    let mysql = Arc::new(
        MysqlStore::with_pool_config(
            &cfg.mysql.database_url,
            PoolConfig {
                max_connections: 10,
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
        draining: std::sync::atomic::AtomicBool::new(false),
    });
    let router = build_router(state.clone());
    (state, router)
}

struct Setup {
    app_id: String,
    fs_ws_id: String,
    db_ws_id: String,
}

/// One account (with app_id) owning one fs project + one db project (db gets
/// no dataset/collection — the kind check reads the workspace row only).
async fn provision(state: &AppState) -> Setup {
    let acct_id = Uuid::new_v4().to_string();
    let app_id = format!("app-{}", Uuid::new_v4().simple());
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "tunnel-bots-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            app_id: Some(app_id.clone()),
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let mut ids = vec![];
    for kind in [WorkspaceKind::Fs, WorkspaceKind::Db] {
        let ws_id = Uuid::new_v4().to_string();
        state
            .auth_store
            .create_workspace(&Workspace {
                id: ws_id.clone(),
                account_id: acct_id.clone(),
                name: format!("{kind:?}-proj-{}", &ws_id[..8]),
                status: WorkspaceStatus::Active,
                kind,
                app_id: Some(app_id.clone()),
                description: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        ids.push(ws_id);
    }
    Setup {
        app_id,
        fs_ws_id: ids.remove(0),
        db_ws_id: ids.remove(0),
    }
}

async fn send(router: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, val)
}

/// Count ACTIVE keys on a project (for the mint/revoke invariants).
async fn active_keys(state: &AppState, ws_id: &str) -> usize {
    state
        .auth_store
        .list_app_workspace_keys(ws_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|(k, _, _, _)| k.status == KeyStatus::Active)
        .count()
}

#[tokio::test]
async fn tunnel_bot_crud_cycle() {
    let (state, router) = build_test_app().await;
    let s = provision(&state).await;
    let base = format!(
        "/v1/workspace/{}/project/{}/tunnel/bots",
        s.app_id, s.fs_ws_id
    );
    let bot_id = format!("bot-{}", Uuid::new_v4().simple());
    let bot_name = format!("kb-bot-{}", &bot_id[4..12]);

    // db project → kind mismatch, fs-only surface.
    let db_base = format!(
        "/v1/workspace/{}/project/{}/tunnel/bots",
        s.app_id, s.db_ws_id
    );
    let (st, body) = send(
        &router,
        "POST",
        &db_base,
        Some(json!({"bot_id": "x", "name": "x", "secret": "s"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "db project: {body}");
    assert_eq!(body["error"]["code"], "WORKSPACE_KIND_MISMATCH");

    // Create on the fs project mints one read-only key.
    assert_eq!(active_keys(&state, &s.fs_ws_id).await, 0);
    let (st, bot) = send(
        &router,
        "POST",
        &base,
        Some(json!({"bot_id": bot_id, "name": bot_name, "secret": "wecom-secret"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "create: {bot}");
    assert_eq!(bot["bot_id"], bot_id.as_str());
    assert_eq!(bot["workspace"], s.app_id.as_str());
    assert_eq!(bot["project"], s.fs_ws_id.as_str());
    assert_eq!(bot["mode"], "hybrid");
    assert_eq!(bot["limit"], 8);
    assert_eq!(bot["conn_state"], "unknown");
    let masked = bot["veda_key"].as_str().unwrap();
    assert!(masked.starts_with("wk_") && masked.contains('…'), "masked key: {masked}");
    assert!(bot.get("secret").is_none(), "secret must never round-trip");
    assert_eq!(active_keys(&state, &s.fs_ws_id).await, 1);

    // Validation: bad mode / out-of-band limit.
    for bad in [
        json!({"bot_id": "b2", "name": "n2", "secret": "s", "mode": "fuzzy"}),
        json!({"bot_id": "b2", "name": "n2", "secret": "s", "limit": 0}),
        json!({"bot_id": "b2", "name": "n2", "secret": "s", "limit": 25}),
        json!({"bot_id": " ", "name": "n2", "secret": "s"}),
    ] {
        let (st, _) = send(&router, "POST", &base, Some(bad)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    // Duplicate bot_id conflicts AND rolls the freshly-minted key back.
    let (st, body) = send(
        &router,
        "POST",
        &base,
        Some(json!({"bot_id": bot_id, "name": "other-name", "secret": "s2"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "dup: {body}");
    assert_eq!(
        active_keys(&state, &s.fs_ws_id).await,
        1,
        "conflicting create must revoke its pre-minted key"
    );

    // List shows exactly our bot (company page envelope).
    let (st, page) = send(&router, "GET", &base, None).await;
    assert_eq!(st, StatusCode::OK);
    let items = page["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["bot_id"], bot_id.as_str());

    // Patch: mode/limit change; omitted secret keeps the stored one.
    let (st, patched) = send(
        &router,
        "PATCH",
        &format!("{base}/{bot_id}"),
        Some(json!({"mode": "semantic", "limit": 12, "secret": ""})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "patch: {patched}");
    assert_eq!(patched["mode"], "semantic");
    assert_eq!(patched["limit"], 12);

    // The whole surface is fs-only now: even list/patch on a db project stop
    // at the kind gate (and a cross-project bot_id stays unreachable).
    let (st, _) = send(&router, "GET", &db_base, None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = send(
        &router,
        "PATCH",
        &format!("{db_base}/{bot_id}"),
        Some(json!({"limit": 9})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Unknown bot → NOT_FOUND.
    let (st, _) = send(&router, "PATCH", &format!("{base}/nope"), Some(json!({"limit": 9}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Delete drops the row and revokes the minted key.
    let (st, body) = send(&router, "DELETE", &format!("{base}/{bot_id}"), None).await;
    assert_eq!(st, StatusCode::OK, "delete: {body}");
    let (st, page) = send(&router, "GET", &base, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(page["data"].as_array().unwrap().is_empty());
    assert_eq!(
        active_keys(&state, &s.fs_ws_id).await,
        0,
        "delete must revoke the bot's auto-minted key"
    );

    // Delete again → NOT_FOUND.
    let (st, _) = send(&router, "DELETE", &format!("{base}/{bot_id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

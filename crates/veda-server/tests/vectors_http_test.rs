//! Stage 4.5 — HTTP layer roundtrip for the vectors data plane.
//!
//! Runs the real `build_router(AppState)` against real MySQL + real
//! Milvus + real embedding. Uses `tower::ServiceExt::oneshot` to dispatch
//! requests directly (no TCP) — fast and avoids port-binding races.
//!
//! Single happy-path test exercises:
//!   POST /v1/vectors/upsert → /search → /query → /delete → /query (empty)
//!
//! Setup goes through direct store calls (not HTTP) — the auth chain is
//! already covered by Stage 1.5 / 1.7 integration tests.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
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
    Account, AccountStatus, ApiKeyRecord, Dataset, DatasetStatus, KeyStatus, Workspace,
    WorkspaceKind, WorkspaceStatus,
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

async fn build_test_app() -> (Arc<AppState>, Arc<MysqlStore>, axum::Router) {
    let cfg = load_config();
    let mysql = Arc::new(
        MysqlStore::with_pool_config(&cfg.mysql.database_url, PoolConfig::default())
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
        vector_workspace_store: milvus.clone(),
        vector_embedding,
        embedding_dim: cfg.embedding.dimension,
        sql_engine,
        jwt_secret: "test-jwt-secret-not-used-for-vk-tokens-32+chars".into(),
        // `install()` registers the global Prometheus recorder. Each
        // integration test file runs in its own binary, so this is safe
        // here (would panic if called twice in the same process).
        metrics: veda_server::obs::install(),
        metrics_token: None,
        summary_enabled: false,
    });
    let router = build_router(state.clone());
    (state, mysql, router)
}

struct TestSetup {
    acct_id: String,
    ws_id: String,
    token: String,
}

async fn provision_test_account(state: &AppState) -> TestSetup {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "vec-http-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let raw_token = format!("vk_{}", Uuid::new_v4().simple());
    let key_hash = sha256_hex(raw_token.as_bytes());
    state
        .auth_store
        .create_api_key(&ApiKeyRecord {
            id: Uuid::new_v4().to_string(),
            account_id: acct_id.clone(),
            name: "test-token".into(),
            key_hash,
            status: KeyStatus::Active,
            app_id: Some("test-app".into()),
            allowed_workspaces: None,
            expires_at: None,
            created_at: now,
        })
        .await
        .unwrap();

    let ws_id = Uuid::new_v4().to_string();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "vectors-http".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Db,
            app_id: Some("test-app".into()),
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

    TestSetup {
        acct_id,
        ws_id,
        token: raw_token,
    }
}

async fn cleanup(state: &AppState, mysql: &MysqlStore, setup: &TestSetup) {
    let _ = state
        .vector_workspace_store
        .drop_collection(&veda_store::vector_collection_name(&setup.ws_id))
        .await;
    let _ = state
        .auth_store
        .hard_delete_datasets_for_workspace(&setup.ws_id)
        .await;
    let _ = state.auth_store.hard_delete_workspace(&setup.ws_id).await;
    let _ = sqlx::query("DELETE FROM veda_api_keys WHERE account_id = ?")
        .bind(&setup.acct_id)
        .execute(mysql.pool())
        .await;
    let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
        .bind(&setup.acct_id)
        .execute(mysql.pool())
        .await;
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[ignore]
async fn vectors_http_full_roundtrip() {
    let (state, mysql, router) = build_test_app().await;
    let setup = provision_test_account(&state).await;

    let do_post = |uri: &str, body: serde_json::Value| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {}", setup.token))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.clone().oneshot(req)
    };

    // 1. Upsert two records.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": setup.ws_id,
            "records": [
                { "row_key": "rk-http-1", "text": "first vector http test", "meta": {"score": 1} },
                { "row_key": "rk-http-2", "text": "second vector http test", "meta": {"score": 2} },
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "upsert status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["success"], true, "upsert success flag: {v:?}");
    assert_eq!(v["data"]["inserted"].as_array().unwrap().len(), 2);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 2. Search.
    let resp = do_post(
        "/v1/vectors/search",
        json!({
            "workspace_id": setup.ws_id,
            "query": "first vector http test",
            "top_k": 5,
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "search status");
    let v = body_json(resp.into_body()).await;
    let hits = v["data"]["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "search returned no hits");
    let row_keys: std::collections::HashSet<String> = hits
        .iter()
        .filter_map(|h| h["row_key"].as_str().map(String::from))
        .collect();
    assert!(
        row_keys.contains("rk-http-1") || row_keys.contains("rk-http-2"),
        "expected one of our upserted keys in search hits: {row_keys:?}"
    );

    // 3. Query by row_keys.
    let resp = do_post(
        "/v1/vectors/query",
        json!({
            "workspace_id": setup.ws_id,
            "row_keys": ["rk-http-1", "rk-http-2"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "query status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["data"]["hits"].as_array().unwrap().len(), 2);

    // 4. Delete.
    let resp = do_post(
        "/v1/vectors/delete",
        json!({
            "workspace_id": setup.ws_id,
            "row_keys": ["rk-http-1", "rk-http-2"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "delete status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["data"]["accepted_count"], 2);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 5. Confirm gone.
    let resp = do_post(
        "/v1/vectors/query",
        json!({
            "workspace_id": setup.ws_id,
            "row_keys": ["rk-http-1", "rk-http-2"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert!(
        v["data"]["hits"].as_array().unwrap().is_empty(),
        "expected empty hits after delete"
    );

    cleanup(&state, &mysql, &setup).await;
}

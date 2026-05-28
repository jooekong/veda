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
    // Conservative pool. Test binary shares one AppState via OnceCell, so
    // this is the single pool serving 4 tests sequentially; 20 conns leaves
    // headroom for the multi-step HTTP roundtrips without saturating
    // dev-MySQL limits.
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

/// Mega-test: drives all 4 vectors HTTP scenarios in a single tokio
/// runtime. Reason: sqlx connection pools are tied to the runtime that
/// created them; multiple `#[tokio::test]` functions in the same binary
/// each get their own runtime, so a shared (OnceCell) pool times out
/// after the first test's runtime dies. Splitting into separate test
/// files works but multiplies cargo's per-binary linking cost. One mega
/// test with explicit sub-sections is the simplest "simple-effective"
/// answer for v0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn vectors_http_e2e_suite() {
    let (state, mysql, router) = build_test_app().await;

    // Sub-test 1: end-to-end happy path roundtrip (Stage 4.5).
    sub_full_roundtrip(&state, &mysql, router.clone()).await;

    // Sub-test 2: full HTTP provisioning (Stage 5.1).
    sub_provisioning_http_e2e(&state, &mysql, router.clone()).await;

    // Sub-test 3: vectors API rejects fs workspace (Stage 5.1).
    sub_vectors_api_rejects_fs(&state, &mysql, router.clone()).await;

    // Sub-test 4: fs API rejects db workspace (Stage 5.1, closes Stage 1.6).
    sub_fs_api_rejects_db(&state, &mysql, router.clone()).await;

    // Sub-test 5: dataset list pagination cursor (task #5).
    sub_dataset_pagination(&state, &mysql, router.clone()).await;

    // Sub-test 6: upsert idempotency + delete semantics (task #7).
    sub_upsert_idempotency_and_delete_semantics(&state, &mysql, router).await;
}

async fn sub_full_roundtrip(state: &Arc<AppState>, mysql: &MysqlStore, router: axum::Router) {
    let setup = provision_test_account(state).await;

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
                { "id": "rk-http-1", "text": "first vector http test", "meta": {"score": 1} },
                { "id": "rk-http-2", "text": "second vector http test", "meta": {"score": 2} },
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "upsert status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["success"], true, "upsert success flag: {v:?}");
    assert_eq!(v["data"]["ids"].as_array().unwrap().len(), 2);

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
    let ids: std::collections::HashSet<String> = hits
        .iter()
        .filter_map(|h| h["id"].as_str().map(String::from))
        .collect();
    assert!(
        ids.contains("rk-http-1") || ids.contains("rk-http-2"),
        "expected one of our upserted keys in search hits: {ids:?}"
    );

    // 3. Query by ids.
    let resp = do_post(
        "/v1/vectors/query",
        json!({
            "workspace_id": setup.ws_id,
            "ids": ["rk-http-1", "rk-http-2"],
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
            "ids": ["rk-http-1", "rk-http-2"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "delete status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["data"]["delete_count"], 2);

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 5. Confirm gone.
    let resp = do_post(
        "/v1/vectors/query",
        json!({
            "workspace_id": setup.ws_id,
            "ids": ["rk-http-1", "rk-http-2"],
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

    // 6. UUID auto-generation flow (codex review Q10): omit `id`, capture
    //    the server-generated UUID from the response, prove we can query +
    //    delete by that UUID. This is the full closed loop for callers
    //    that don't supply their own ids.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": setup.ws_id,
            "records": [
                { "text": "auto-id record one" },
                { "text": "auto-id record two" },
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "auto-id upsert status");
    let v = body_json(resp.into_body()).await;
    let gen_ids: Vec<String> = v["data"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(gen_ids.len(), 2, "expected 2 auto-generated ids");
    assert!(
        gen_ids.iter().all(|id| !id.is_empty() && id.len() >= 16),
        "auto-generated ids should be UUID-shaped: {gen_ids:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = do_post(
        "/v1/vectors/query",
        json!({"workspace_id": setup.ws_id, "ids": gen_ids}),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["data"]["hits"].as_array().unwrap().len(),
        2,
        "expected to round-trip both auto-id records via query"
    );

    let resp = do_post(
        "/v1/vectors/delete",
        json!({"workspace_id": setup.ws_id, "ids": gen_ids}),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    cleanup(&state, &mysql, &setup).await;
}

// ── Stage 5.1: E2E + fs regression ───────────────────────────────────

/// Create account + vk_ token via direct store calls (no workspace).
/// Used by tests that want to drive workspace creation via the HTTP path.
async fn provision_account_only(state: &AppState) -> (String, String) {
    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "stage-5-1".into(),
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
            name: "test-vk".into(),
            key_hash,
            status: KeyStatus::Active,
            app_id: None,
            allowed_workspaces: None,
            expires_at: None,
            created_at: now,
        })
        .await
        .unwrap();
    (acct_id, raw_token)
}

async fn cleanup_account_only(mysql: &MysqlStore, acct_id: &str) {
    let _ = sqlx::query("DELETE FROM veda_workspace_keys WHERE workspace_id IN (SELECT id FROM veda_workspaces WHERE account_id = ?)")
        .bind(acct_id)
        .execute(mysql.pool())
        .await;
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE account_id = ?")
        .bind(acct_id)
        .execute(mysql.pool())
        .await;
    let _ = sqlx::query("DELETE FROM veda_api_keys WHERE account_id = ?")
        .bind(acct_id)
        .execute(mysql.pool())
        .await;
    let _ = sqlx::query("DELETE FROM veda_accounts WHERE id = ?")
        .bind(acct_id)
        .execute(mysql.pool())
        .await;
}

/// Stage 5.1 — drive a db-workspace through the HTTP provisioning path
/// (POST /v1/workspaces with kind=db). End-to-end exercise:
/// account auth → workspace creation (which triggers provision_db_workspace,
/// hence Milvus collection create + default dataset bootstrap) → immediate
/// vectors upsert against the new workspace. If provisioning is silently
/// skipping a step, the upsert fails clearly.
async fn sub_provisioning_http_e2e(
    state: &Arc<AppState>,
    mysql: &MysqlStore,
    router: axum::Router,
) {
    let (acct_id, token) = provision_account_only(state).await;

    let do_post = |uri: &str, body: serde_json::Value| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.clone().oneshot(req)
    };

    let resp = do_post(
        "/v1/workspaces",
        json!({
            "name": "http-e2e-db-ws",
            "kind": "db",
            "app_id": "test-app",
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "workspace create status");
    let v = body_json(resp.into_body()).await;
    let ws_id = v["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["data"]["kind"], "db");

    // Upsert against the newly-provisioned workspace — verifies Milvus
    // collection + default dataset row were both created during the
    // HTTP POST above.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": ws_id,
            "records": [{ "id": "rk-e2e-prov", "text": "provisioning end to end" }],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "upsert post-provisioning");

    // Cleanup.
    let _ = state
        .vector_workspace_store
        .drop_collection(&veda_store::vector_collection_name(&ws_id))
        .await;
    let _ = state
        .auth_store
        .hard_delete_datasets_for_workspace(&ws_id)
        .await;
    let _ = state.auth_store.hard_delete_workspace(&ws_id).await;
    cleanup_account_only(&mysql, &acct_id).await;
}

/// Stage 5.1 — vectors API must reject fs-kind workspaces.
/// Without this, a caller could mistakenly point /v1/vectors/upsert at
/// an fs workspace and the handler would NOT find a Milvus collection
/// (because fs workspaces don't provision one) — opaque downstream
/// errors. Better: refuse at auth time with 400 kind_mismatch.
async fn sub_vectors_api_rejects_fs(
    state: &Arc<AppState>,
    mysql: &MysqlStore,
    router: axum::Router,
) {
    let (acct_id, token) = provision_account_only(state).await;

    let do_post = |uri: &str, body: serde_json::Value| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.clone().oneshot(req)
    };

    // Create an fs workspace.
    let resp = do_post(
        "/v1/workspaces",
        json!({
            "name": "fs-ws-for-isolation",
            "kind": "fs",
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let fs_ws_id = v["data"]["id"].as_str().unwrap().to_string();

    // Try /v1/vectors/upsert with the fs workspace — expect 400.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": fs_ws_id,
            "records": [{ "text": "should be rejected" }],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vectors API must reject fs workspace"
    );
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["error_code"].as_str(),
        Some("WORKSPACE_KIND_MISMATCH"),
        "expected error_code=WORKSPACE_KIND_MISMATCH, got: {v:?}"
    );

    // Cleanup.
    let _ = state.auth_store.hard_delete_workspace(&fs_ws_id).await;
    cleanup_account_only(&mysql, &acct_id).await;
}

/// Stage 5.1 — fs API must reject db-kind workspaces.
/// Covers the deferred Stage 1.6 HTTP-layer check: `AuthWorkspace`
/// extractor enforces `kind == Fs` and returns 400 when a wk_ token
/// scoped to a db workspace is presented on an fs endpoint.
async fn sub_fs_api_rejects_db(
    state: &Arc<AppState>,
    mysql: &MysqlStore,
    router: axum::Router,
) {
    let (acct_id, token) = provision_account_only(state).await;

    let do_post = |uri: &str, body: serde_json::Value, bearer: &str| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.clone().oneshot(req)
    };

    // Create a db workspace.
    let resp = do_post(
        "/v1/workspaces",
        json!({
            "name": "db-ws-for-fs-reject",
            "kind": "db",
            "app_id": "test",
        }),
        &token,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let db_ws_id = v["data"]["id"].as_str().unwrap().to_string();

    // Issue a wk_ token for the db workspace. `create_workspace_key`
    // doesn't check kind — that's by design; kind enforcement happens
    // at use-time on the fs API path.
    let resp = do_post(
        &format!("/v1/workspaces/{db_ws_id}/keys"),
        json!({ "name": "test-wk", "permission": "readwrite" }),
        &token,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let wk_token = v["data"]["key"].as_str().unwrap().to_string();

    // Use the wk_ on an fs endpoint — AuthWorkspace extractor must
    // reject with 400 workspace_kind_mismatch.
    let resp = do_post(
        "/v1/search",
        json!({ "query": "anything" }),
        &wk_token,
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "fs API must reject db workspace via wk_ token; got {}",
        resp.status()
    );
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["error_code"].as_str(),
        Some("WORKSPACE_KIND_MISMATCH"),
        "expected error_code=WORKSPACE_KIND_MISMATCH, got: {v:?}"
    );

    // Cleanup.
    let _ = state
        .vector_workspace_store
        .drop_collection(&veda_store::vector_collection_name(&db_ws_id))
        .await;
    let _ = state
        .auth_store
        .hard_delete_datasets_for_workspace(&db_ws_id)
        .await;
    let _ = state.auth_store.hard_delete_workspace(&db_ws_id).await;
    cleanup_account_only(&mysql, &acct_id).await;
}

/// Task #5 — cursor pagination on `GET /v1/workspaces/{ws}/datasets`.
/// Creates several extra datasets so the default page can be split,
/// then walks the pages via `?limit=&after=` and verifies the full set
/// is reachable exactly once across pages.
async fn sub_dataset_pagination(
    state: &Arc<AppState>,
    mysql: &MysqlStore,
    router: axum::Router,
) {
    let setup = provision_test_account(state).await;
    let token = setup.token.clone();
    let ws_id = setup.ws_id.clone();

    let do_post = |uri: String, body: serde_json::Value| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        router.clone().oneshot(req)
    };
    let do_get = |uri: String| {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        router.clone().oneshot(req)
    };

    // Create 3 extra datasets — together with the bootstrapped `default`
    // we have 4 total. Names are intentionally varied so id sort != name
    // sort (proves the ordering claim is "id ASC").
    for name in ["alpha", "delta", "gamma"] {
        let resp = do_post(
            format!("/v1/workspaces/{ws_id}/datasets"),
            json!({ "name": name }),
        )
        .await
        .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "create dataset {name} failed",
        );
    }

    // Walk pages with limit=2; expect 2+2 = 4 items total, has_more flips
    // false on the second page.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for round in 0..5 {
        assert!(round < 4, "pagination did not terminate after 4 rounds");
        let uri = match &cursor {
            Some(c) => format!("/v1/workspaces/{ws_id}/datasets?limit=2&after={c}"),
            None => format!("/v1/workspaces/{ws_id}/datasets?limit=2"),
        };
        let resp = do_get(uri).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp.into_body()).await;
        let items = v["data"]["items"].as_array().expect("items array");
        for it in items {
            seen.push(it["name"].as_str().unwrap().to_string());
        }
        let has_more = v["data"]["has_more"].as_bool().unwrap();
        let next_cursor = v["data"]["next_cursor"].as_str().map(String::from);
        if has_more {
            assert!(
                next_cursor.is_some(),
                "has_more=true must come with next_cursor: {v:?}"
            );
            cursor = next_cursor;
        } else {
            assert!(
                next_cursor.is_none(),
                "has_more=false must omit next_cursor: {v:?}"
            );
            break;
        }
    }

    let seen_set: std::collections::HashSet<_> = seen.iter().cloned().collect();
    assert_eq!(
        seen_set.len(),
        seen.len(),
        "duplicate dataset in pagination: {seen:?}"
    );
    for expected in ["default", "alpha", "delta", "gamma"] {
        assert!(
            seen_set.contains(expected),
            "missing {expected} across pages: {seen_set:?}"
        );
    }

    cleanup(state, mysql, &setup).await;
}

/// Task #7 — upsert idempotency contract + delete semantics.
/// Three contracts in one sub-test:
///   1. Same-batch duplicate `id`: server-side dedupe, last-wins value
///      at first-occurrence position (Milvus 2.6 rejects same-batch dup
///      PKs with code 1100, so handler must collapse).
///   2. Cross-call same `id`: idempotent replace, query shows the latest
///      payload, no duplicate rows.
///   3. Delete with mixed-existence ids: `delete_count` always equals
///      `len(ids)` per Milvus tombstone model (locks the v0 contract
///      claimed in docs/api/vectors.md).
async fn sub_upsert_idempotency_and_delete_semantics(
    state: &Arc<AppState>,
    mysql: &MysqlStore,
    router: axum::Router,
) {
    let setup = provision_test_account(state).await;
    let token = setup.token.clone();
    let ws_id = setup.ws_id.clone();

    let do_post = |uri: &str, body: serde_json::Value| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        router.clone().oneshot(req)
    };

    // 1. Same-batch duplicate id — server-side dedupe, last entry wins.
    //    Milvus 2.6 rejects same-batch dup PKs (code 1100), so handler
    //    collapses before sending. Response `ids` reflects deduped count.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": ws_id,
            "records": [
                { "id": "dup-1", "text": "first", "meta": {"version": 1} },
                { "id": "other", "text": "unrelated", "meta": {"version": 0} },
                { "id": "dup-1", "text": "second", "meta": {"version": 2} },
                { "id": "dup-1", "text": "third (winner)", "meta": {"version": 3} },
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "duplicate-id batch must not 4xx");
    let v = body_json(resp.into_body()).await;
    let ids: Vec<String> = v["data"]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["dup-1".to_string(), "other".to_string()],
        "deduped ids preserve first-occurrence position; got {ids:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = do_post(
        "/v1/vectors/query",
        json!({"workspace_id": ws_id, "ids": ["dup-1"]}),
    )
    .await
    .unwrap();
    let v = body_json(resp.into_body()).await;
    let hits = v["data"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "must be exactly one row after PK collapse");
    assert_eq!(
        hits[0]["text"].as_str(),
        Some("third (winner)"),
        "last entry must win"
    );
    assert_eq!(
        hits[0]["meta"]["version"].as_i64(),
        Some(3),
        "winner's meta also wins"
    );

    // 2. Cross-call same id — idempotent replace.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": ws_id,
            "records": [{ "id": "idem-1", "text": "v1", "meta": {"v": 1} }],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": ws_id,
            "records": [{ "id": "idem-1", "text": "v2", "meta": {"v": 2} }],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let resp = do_post(
        "/v1/vectors/query",
        json!({"workspace_id": ws_id, "ids": ["idem-1"]}),
    )
    .await
    .unwrap();
    let v = body_json(resp.into_body()).await;
    let hits = v["data"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "idempotent replace, not duplicate rows");
    assert_eq!(hits[0]["text"].as_str(), Some("v2"));
    assert_eq!(hits[0]["meta"]["v"].as_i64(), Some(2));

    // 3. Mixed-existence delete: locks the public contract that
    //    delete_count = len(ids), independent of physical existence.
    //    (Three ids exist: dup-1, other, idem-1. Two don't: ghost-a, ghost-b.)
    let resp = do_post(
        "/v1/vectors/delete",
        json!({
            "workspace_id": ws_id,
            "ids": ["dup-1", "other", "idem-1", "ghost-a", "ghost-b"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["data"]["delete_count"].as_u64(),
        Some(5),
        "delete_count must equal len(ids) (Milvus tombstone marker count, NOT physical-existence count)"
    );

    cleanup(state, mysql, &setup).await;
}

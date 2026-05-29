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
    sub_upsert_idempotency_and_delete_semantics(&state, &mysql, router.clone()).await;

    // Sub-test 7: admin token minting + allowed_workspaces scope + disable.
    sub_admin_tokens_scope(&state, &mysql, router.clone()).await;

    // Sub-test 8: dataset delete guard incl. case-insensitive default (S5).
    sub_dataset_delete_guard(&state, &mysql, router.clone()).await;

    // Sub-test 9: accounts register + login (production account.rs handlers).
    sub_accounts_auth(&state, &mysql, router.clone()).await;

    // Sub-test 10: output_fields projection whitelist (search/query).
    sub_projection(&state, &mysql, router).await;
}

/// `output_fields` projection: id/score always returned, selected fields
/// projected in, the rest absent from the wire JSON; internal columns
/// rejected with 400. Closes the Stage-4 wire contract for projection.
async fn sub_projection(state: &Arc<AppState>, mysql: &MysqlStore, router: axum::Router) {
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

    // Upsert one fully-populated record.
    let resp = do_post(
        "/v1/vectors/upsert",
        json!({
            "workspace_id": setup.ws_id,
            "records": [{
                "id": "proj-1",
                "text": "projection test body",
                "category": "cat-x",
                "tags": ["t1", "t2"],
                "meta": {"k": "v"},
            }],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "projection upsert status");

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // search output_fields=["text"] → id + score + text only.
    let resp = do_post(
        "/v1/vectors/search",
        json!({
            "workspace_id": setup.ws_id,
            "query": "projection test body",
            "top_k": 5,
            "output_fields": ["text"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let hit = &v["data"]["hits"][0];
    assert!(hit["id"].is_string(), "id always returned: {hit:?}");
    assert!(hit["score"].is_number(), "score always returned");
    assert!(hit["text"].is_string(), "text projected in");
    assert!(hit["meta"].is_null(), "meta projected out");
    assert!(hit["dataset"].is_null(), "dataset projected out");
    assert!(hit["category"].is_null(), "category projected out");
    assert!(hit["tags"].is_null(), "tags projected out");
    assert!(hit["created_at"].is_null(), "created_at projected out");

    // search output_fields=[] → only id + score.
    let resp = do_post(
        "/v1/vectors/search",
        json!({
            "workspace_id": setup.ws_id,
            "query": "projection test body",
            "output_fields": [],
        }),
    )
    .await
    .unwrap();
    let v = body_json(resp.into_body()).await;
    let hit = &v["data"]["hits"][0];
    assert!(hit["id"].is_string());
    assert!(hit["score"].is_number());
    assert!(hit["text"].is_null(), "empty output_fields → no text");
    assert!(hit["meta"].is_null());

    // query output_fields=["meta"] → id + meta, no text.
    let resp = do_post(
        "/v1/vectors/query",
        json!({
            "workspace_id": setup.ws_id,
            "ids": ["proj-1"],
            "output_fields": ["meta"],
        }),
    )
    .await
    .unwrap();
    let v = body_json(resp.into_body()).await;
    let hit = &v["data"]["hits"][0];
    assert!(hit["id"].is_string());
    assert_eq!(hit["meta"]["k"].as_str(), Some("v"), "meta projected in");
    assert!(hit["text"].is_null(), "text projected out on query");

    // whitelist: projecting an internal column → 400.
    let resp = do_post(
        "/v1/vectors/search",
        json!({
            "workspace_id": setup.ws_id,
            "query": "x",
            "output_fields": ["pk"],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "projecting internal column pk must be rejected"
    );

    cleanup(state, mysql, &setup).await;
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

/// Task #8 — `POST /admin/v1/tokens` minting + `allowed_workspaces` scope
/// enforcement + disable. The auth-critical token path had no HTTP e2e: the
/// other sub-tests mint tokens by inserting `veda_api_keys` rows directly,
/// bypassing the production handler entirely. This drives the real flow.
async fn sub_admin_tokens_scope(state: &Arc<AppState>, mysql: &MysqlStore, router: axum::Router) {
    // setup: unrestricted account token + a db workspace WITH a real Milvus
    // collection (so an in-scope query reaches the data plane and returns 200).
    let setup = provision_test_account(state).await;

    // A second db workspace under the SAME account = the out-of-scope target.
    // No collection needed: load_db_workspace runs check_workspace_allowed
    // BEFORE touching Milvus, so the 403 fires first. Same-account ownership
    // isolates the allowed_workspaces check from the ownership check.
    let other_ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: other_ws_id.clone(),
            account_id: setup.acct_id.clone(),
            name: "out-of-scope-ws".into(),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Db,
            app_id: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

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

    // 1. Mint a service token scoped to ONLY setup.ws_id (caller = account token).
    let resp = do_post(
        "/admin/v1/tokens",
        json!({ "app_id": "scoped-app", "name": "prod", "allowed_workspaces": [setup.ws_id] }),
        &setup.token,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "mint token status");
    let v = body_json(resp.into_body()).await;
    let token_id = v["data"]["id"].as_str().unwrap().to_string();
    let scoped = v["data"]["token"].as_str().unwrap().to_string();
    assert!(scoped.starts_with("vk_"), "minted token must be vk_: {scoped}");

    // 2. Scoped token reaches the in-scope workspace (200; empty hits is fine).
    let resp = do_post(
        "/v1/vectors/query",
        json!({ "workspace_id": setup.ws_id, "ids": ["nope"] }),
        &scoped,
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "scoped token must reach in-scope workspace"
    );

    // 3. Scoped token is denied the out-of-scope workspace → 403.
    let resp = do_post(
        "/v1/vectors/query",
        json!({ "workspace_id": other_ws_id, "ids": ["nope"] }),
        &scoped,
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "out-of-scope workspace must be denied"
    );
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error_code"].as_str(), Some("PERMISSION_DENIED"), "{v:?}");

    // 4. Disable → 204; afterwards the token no longer authenticates (401).
    let resp = do_post(
        &format!("/admin/v1/tokens/{token_id}/disable"),
        json!({}),
        &setup.token,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "disable status");

    let resp = do_post(
        "/v1/vectors/query",
        json!({ "workspace_id": setup.ws_id, "ids": ["nope"] }),
        &scoped,
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "disabled token must stop authenticating"
    );

    let _ = state.auth_store.hard_delete_workspace(&other_ws_id).await;
    cleanup(state, mysql, &setup).await;
}

/// S5 — `DELETE /v1/workspaces/{ws}/datasets/{name}`, the only delete path
/// with no prior e2e. Covers normal archive (204), the `default` guard, the
/// case-insensitive `default` guard (MySQL `utf8mb4_0900_ai_ci` would
/// otherwise let `Default` archive the `default` row), and missing → 404.
async fn sub_dataset_delete_guard(state: &Arc<AppState>, mysql: &MysqlStore, router: axum::Router) {
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
    let do_delete = |uri: String| {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        router.clone().oneshot(req)
    };

    // Create a removable dataset, then archive it → 204.
    let resp = do_post(
        format!("/v1/workspaces/{ws_id}/datasets"),
        json!({ "name": "removable" }),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create removable dataset");

    let resp = do_delete(format!("/v1/workspaces/{ws_id}/datasets/removable"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "archive removable → 204");

    // default guard — exact lowercase.
    let resp = do_delete(format!("/v1/workspaces/{ws_id}/datasets/default"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "default must be protected");
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["error_code"].as_str(),
        Some("CANNOT_DELETE_DEFAULT_DATASET"),
        "{v:?}"
    );

    // default guard — case-insensitive (S5). The handler must reject `Default`
    // BEFORE archive_dataset runs, else the ci collation archives `default`.
    let resp = do_delete(format!("/v1/workspaces/{ws_id}/datasets/Default"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Default (mixed case) must also be protected (S5)"
    );
    let v = body_json(resp.into_body()).await;
    assert_eq!(
        v["error_code"].as_str(),
        Some("CANNOT_DELETE_DEFAULT_DATASET"),
        "case-insensitive default guard: {v:?}"
    );

    // Missing dataset → 404.
    let resp = do_delete(format!("/v1/workspaces/{ws_id}/datasets/nonexistent"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "missing dataset → 404");

    // Prove `default` survived the `Default` attempt (S5: must NOT have been
    // archived through the ci-collation match).
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/workspaces/{ws_id}/datasets"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp.into_body()).await;
    let names: Vec<String> = v["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "default"),
        "default dataset must survive the Default delete attempt: {names:?}"
    );

    cleanup(state, mysql, &setup).await;
}

/// Accounts register + login against the production `account.rs` handlers
/// (server_test.rs exercises mock handlers; this file otherwise mints via
/// direct store inserts). Covers registration, duplicate-email conflict,
/// login success, wrong-password rejection, and that the returned key works.
async fn sub_accounts_auth(_state: &Arc<AppState>, mysql: &MysqlStore, router: axum::Router) {
    let email = format!("acct-{}@test.com", Uuid::new_v4().simple());
    let password = "correct-horse-battery";

    let do_post = |uri: &str, body: serde_json::Value, bearer: Option<&str>| {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        let req = req
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        router.clone().oneshot(req)
    };

    // 1. Register → 200 + account_id + vk_ api_key.
    let resp = do_post(
        "/v1/accounts",
        json!({ "name": "acct-auth-test", "email": email, "password": password }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "register status");
    let v = body_json(resp.into_body()).await;
    let acct_id = v["data"]["account_id"].as_str().unwrap().to_string();
    let api_key = v["data"]["api_key"].as_str().unwrap().to_string();
    assert!(api_key.starts_with("vk_"), "api_key must be vk_: {api_key}");

    // 2. Duplicate email → 409.
    let resp = do_post(
        "/v1/accounts",
        json!({ "name": "dup", "email": email, "password": "another" }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "duplicate email → 409");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["error_code"].as_str(), Some("ALREADY_EXISTS"), "{v:?}");

    // 3. Login with correct password → 200, same account.
    let resp = do_post(
        "/v1/accounts/login",
        json!({ "email": email, "password": password }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login status");
    let v = body_json(resp.into_body()).await;
    assert_eq!(v["data"]["account_id"].as_str(), Some(acct_id.as_str()));

    // 4. Login with wrong password → 401.
    let resp = do_post(
        "/v1/accounts/login",
        json!({ "email": email, "password": "wrong" }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "wrong password → 401");

    // 5. The registration key actually authorizes a real endpoint.
    let resp = do_post(
        "/v1/workspaces",
        json!({ "name": "from-registered-key", "kind": "fs" }),
        Some(&api_key),
    )
    .await
    .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "registered api_key must authorize workspace creation"
    );

    cleanup_account_only(mysql, &acct_id).await;
}

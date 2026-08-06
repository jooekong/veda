//! Platform tunnel QA telemetry (`.../tunnel/qa/{stats,logs}`).
//!
//! Real MySQL via `build_router(AppState)` + `tower::ServiceExt::oneshot`
//! (Milvus/embedding are wired but untouched — this surface is pure MySQL).
//! Seeds `veda_tunnel_qa_log` / `veda_tunnel_qa_feedback` rows directly (the
//! tunnel writes them in prod), then asserts the platform read surface. The
//! headline invariant is **tenant isolation**: project A never sees project
//! B's bot rows, and a foreign `bot_id` collapses to NOT_FOUND.
//!
//! No `VEDA_PLATFORM_BASE` → external authz skipped (same as the other
//! platform-surface tests). Rows are keyed by per-run UUID bot_ids so the
//! shared `veda_it` DB never cross-contaminates, and the seeded rows are
//! cleaned up at the end.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::mysql::MySqlPool;
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
use veda_types::{Account, AccountStatus, Workspace, WorkspaceKind, WorkspaceStatus};

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

/// Returns the app + a direct pool (same DB) for seeding QA rows.
async fn build_test_app() -> (Arc<AppState>, axum::Router, MySqlPool) {
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

    let seed_pool = MySqlPool::connect(&cfg.mysql.database_url)
        .await
        .expect("seed pool connect");

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
    (state, router, seed_pool)
}

async fn new_account(state: &AppState) -> (String, String) {
    let acct_id = Uuid::new_v4().to_string();
    let app_id = format!("app-{}", Uuid::new_v4().simple());
    let now = Utc::now();
    state
        .auth_store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "tunnel-qa-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            app_id: Some(app_id.clone()),
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    (acct_id, app_id)
}

async fn new_ws(state: &AppState, acct_id: &str, app_id: &str, kind: WorkspaceKind) -> String {
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.to_string(),
            name: format!("{kind:?}-{}", &ws_id[..8]),
            status: WorkspaceStatus::Active,
            kind,
            app_id: Some(app_id.to_string()),
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    ws_id
}

async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path);
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

/// Attach a bot to a project through the real API (mints the key, writes the
/// row); returns the chosen bot_id.
async fn attach_bot(router: &axum::Router, app_id: &str, ws_id: &str) -> String {
    let bot_id = format!("bot-{}", Uuid::new_v4().simple());
    let base = format!("/v1/workspace/{app_id}/project/{ws_id}/tunnel/bots");
    let (st, body) = send(
        router,
        "POST",
        &base,
        Some(json!({"bot_id": bot_id, "name": format!("n-{bot_id}"), "secret": "s"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "attach bot: {body}");
    bot_id
}

/// Insert one qa_log row; returns its id. `feedback_id` links to a vote row.
async fn seed_log(
    pool: &MySqlPool,
    bot_id: &str,
    outcome: &str,
    query: &str,
    answer: &str,
    feedback_id: Option<&str>,
) -> u64 {
    let res = sqlx::query(
        "INSERT INTO veda_tunnel_qa_log \
         (bot_id, chat_type, chat_key, user_id, query, outcome, hit_count, citation_count, \
          latency_ms, answer_text, feedback_id) \
         VALUES (?, 'single', 'chatkey', 'user-1', ?, ?, 2, 1, 120, ?, ?)",
    )
    .bind(bot_id)
    .bind(query)
    .bind(outcome)
    .bind(answer)
    .bind(feedback_id)
    .execute(pool)
    .await
    .unwrap();
    res.last_insert_id()
}

async fn seed_vote(pool: &MySqlPool, feedback_id: &str, user_id: &str, kind: i8) {
    sqlx::query(
        "INSERT INTO veda_tunnel_qa_feedback (feedback_id, user_id, kind) VALUES (?, ?, ?)",
    )
    .bind(feedback_id)
    .bind(user_id)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &MySqlPool, bot_ids: &[&str], feedback_ids: &[&str]) {
    for b in bot_ids {
        let _ = sqlx::query("DELETE FROM veda_tunnel_qa_log WHERE bot_id = ?")
            .bind(b)
            .execute(pool)
            .await;
    }
    for f in feedback_ids {
        let _ = sqlx::query("DELETE FROM veda_tunnel_qa_feedback WHERE feedback_id = ?")
            .bind(f)
            .execute(pool)
            .await;
    }
}

#[tokio::test]
async fn tunnel_qa_stats_and_logs() {
    let (state, router, pool) = build_test_app().await;
    let (acct, app) = new_account(&state).await;
    let ws_a = new_ws(&state, &acct, &app, WorkspaceKind::Fs).await;
    let ws_b = new_ws(&state, &acct, &app, WorkspaceKind::Fs).await;
    let ws_empty = new_ws(&state, &acct, &app, WorkspaceKind::Fs).await;
    let ws_db = new_ws(&state, &acct, &app, WorkspaceKind::Db).await;

    let bot_a = attach_bot(&router, &app, &ws_a).await;
    let bot_b = attach_bot(&router, &app, &ws_b).await;

    // Unique feedback ids for this run.
    let fa1 = format!("fa1-{}", Uuid::new_v4().simple());
    let fa2 = format!("fa2-{}", Uuid::new_v4().simple());
    let fb1 = format!("fb1-{}", Uuid::new_v4().simple());

    // Project A: 3 logs (answered↑, no_context, answered↓).
    seed_log(&pool, &bot_a, "answered", "q-a-1", "answer one", Some(&fa1)).await;
    seed_log(&pool, &bot_a, "no_context", "q-a-2", "no ctx", None).await;
    let last_a = seed_log(&pool, &bot_a, "answered", "q-a-3", "answer three", Some(&fa2)).await;
    // Give the newest row a retrieval trace — the API must serialize it as
    // a parsed ARRAY (workbench renders steps directly), not a JSON string.
    sqlx::query("UPDATE veda_tunnel_qa_log SET tool_trace = ? WHERE id = ?")
        .bind(r#"[{"tool":"search","detail":"DAL 接入"},{"tool":"read_file","detail":"/index.md"}]"#)
        .bind(last_a)
        .execute(&pool)
        .await
        .unwrap();
    seed_vote(&pool, &fa1, "voter-1", 1).await; // up
    seed_vote(&pool, &fa2, "voter-2", 2).await; // down

    // Project B: 1 log (error↓) — must never bleed into A.
    seed_log(&pool, &bot_b, "error", "q-b-1", "kb down", Some(&fb1)).await;
    seed_vote(&pool, &fb1, "voter-3", 2).await;

    // ── stats(A): only A's bots counted ───────────────────────────────
    let stats_a = format!("/v1/workspace/{app}/project/{ws_a}/tunnel/qa/stats");
    let (st, s) = send(&router, "GET", &stats_a, None).await;
    assert_eq!(st, StatusCode::OK, "stats A: {s}");
    assert_eq!(s["days"], 7, "default window");
    assert_eq!(s["total"], 3);
    assert_eq!(s["outcomes"]["answered"], 2);
    assert_eq!(s["outcomes"]["no_context"], 1);
    assert!(s["outcomes"].get("error").is_none(), "B's error must not leak: {s}");
    assert_eq!(s["feedback_up"], 1);
    assert_eq!(s["feedback_down"], 1);

    // ── stats(B): only B's bots counted ───────────────────────────────
    let stats_b = format!("/v1/workspace/{app}/project/{ws_b}/tunnel/qa/stats");
    let (st, s) = send(&router, "GET", &stats_b, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s["total"], 1);
    assert_eq!(s["outcomes"]["error"], 1);
    assert!(s["outcomes"].get("answered").is_none(), "A's rows must not leak into B");
    assert_eq!(s["feedback_down"], 1);

    // ── logs(A): company page envelope, newest first ──────────────────
    let logs_a = format!("/v1/workspace/{app}/project/{ws_a}/tunnel/qa/logs");
    let (st, p) = send(&router, "GET", &logs_a, None).await;
    assert_eq!(st, StatusCode::OK, "logs A: {p}");
    assert_eq!(p["total"], 3);
    assert_eq!(p["page"], 1);
    assert_eq!(p["size"], 20);
    assert_eq!(p["has_next_page"], false);
    let rows = p["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    // Newest (highest id) first, answer_text verbatim.
    assert_eq!(rows[0]["id"].as_u64().unwrap(), last_a);
    assert_eq!(rows[0]["query"], "q-a-3");
    assert_eq!(rows[0]["answer_text"], "answer three");
    assert_eq!(rows[0]["down_count"], 1);
    assert_eq!(rows[0]["bot_id"], bot_a.as_str());
    // tool_trace comes back as a STRUCTURED array (not a string needing a
    // second parse); rows without a trace serialize as null.
    let trace = rows[0]["tool_trace"]
        .as_array()
        .expect("tool_trace must be a parsed array");
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0]["tool"], "search");
    assert_eq!(trace[1]["detail"], "/index.md");
    assert!(rows[1]["tool_trace"].is_null(), "traceless rows → null");
    for r in rows {
        assert_eq!(r["bot_id"], bot_a.as_str(), "no B rows in A's list");
    }

    // ── logs(A) pagination: size=2 splits 3 rows across 2 pages ───────
    let (_, p1) = send(&router, "GET", &format!("{logs_a}?size=2&page=1"), None).await;
    assert_eq!(p1["total"], 3);
    assert_eq!(p1["data"].as_array().unwrap().len(), 2);
    assert_eq!(p1["has_next_page"], true);
    let (_, p2) = send(&router, "GET", &format!("{logs_a}?size=2&page=2"), None).await;
    assert_eq!(p2["data"].as_array().unwrap().len(), 1);
    assert_eq!(p2["has_next_page"], false);
    assert_eq!(p2["has_prev_page"], true);

    // ── logs(A) filters: down_voted + outcome ─────────────────────────
    let (_, dv) = send(&router, "GET", &format!("{logs_a}?down_voted=true"), None).await;
    assert_eq!(dv["total"], 1, "only the down-voted answered row");
    assert_eq!(dv["data"][0]["query"], "q-a-3");
    let (_, oc) = send(&router, "GET", &format!("{logs_a}?outcome=no_context"), None).await;
    assert_eq!(oc["total"], 1);
    assert_eq!(oc["data"][0]["query"], "q-a-2");

    // Unknown outcome → 400 INVALID_INPUT (typo guard).
    let (st, e) = send(&router, "GET", &format!("{logs_a}?outcome=bogus"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "bogus outcome: {e}");
    assert_eq!(e["error"]["code"], "INVALID_INPUT");

    // ── tenant isolation: A cannot address B's bot ────────────────────
    let (st, e) = send(&router, "GET", &format!("{logs_a}?bot_id={bot_b}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "foreign bot_id in logs: {e}");
    assert_eq!(e["error"]["code"], "NOT_FOUND");
    let (st, _) = send(&router, "GET", &format!("{stats_a}?bot_id={bot_b}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "foreign bot_id in stats");
    // A's own bot as a filter still works.
    let (st, s) = send(&router, "GET", &format!("{stats_a}?bot_id={bot_a}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s["total"], 3);

    // ── empty project: no bots → empty stats / list, not an error ─────
    let stats_e = format!("/v1/workspace/{app}/project/{ws_empty}/tunnel/qa/stats");
    let (st, s) = send(&router, "GET", &stats_e, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s["total"], 0);
    assert_eq!(s["feedback_up"], 0);
    assert!(s["outcomes"].as_object().unwrap().is_empty());
    let logs_e = format!("/v1/workspace/{app}/project/{ws_empty}/tunnel/qa/logs");
    let (st, p) = send(&router, "GET", &logs_e, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(p["total"], 0);
    assert!(p["data"].as_array().unwrap().is_empty());

    // ── db project: fs-only surface → WORKSPACE_KIND_MISMATCH ─────────
    let stats_db = format!("/v1/workspace/{app}/project/{ws_db}/tunnel/qa/stats");
    let (st, e) = send(&router, "GET", &stats_db, None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "db stats: {e}");
    assert_eq!(e["error"]["code"], "WORKSPACE_KIND_MISMATCH");

    // ── days clamp: days=1000 accepted, clamped to 90 ─────────────────
    let (st, s) = send(&router, "GET", &format!("{stats_a}?days=1000"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s["days"], 90);

    // ── cross-tenant project id → NOT_FOUND (not another tenant's data) ─
    let (_, app2) = new_account(&state).await;
    let foreign = format!("/v1/workspace/{app2}/project/{ws_a}/tunnel/qa/stats");
    let (st, _) = send(&router, "GET", &foreign, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "A's project id under another workspace");

    cleanup(&pool, &[&bot_a, &bot_b], &[&fa1, &fa2, &fb1]).await;
}

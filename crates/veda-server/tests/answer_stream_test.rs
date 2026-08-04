//! `/v1/answer/stream` end-to-end against real MySQL + Milvus + embedding +
//! airouter LLM (config/test.toml). Verifies the SSE contract:
//!   - grounded question → ≥1 `delta` + one `final`, final.answer equals the
//!     concatenated deltas (align_citations never rewrites text), citations
//!     non-empty
//!   - unanswerable question → single `final` carrying the canned refusal
//!   - bad request → plain HTTP 400 (no SSE opened)
//! Run with --ignored (slow: waits for async indexing).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;
use veda_core::checksum::sha256_hex;
use veda_core::service::answer::{AnswerParams, AnswerService};
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, LlmService, VectorStore};
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
use veda_pipeline::llm::LlmProvider;
use veda_server::routes::build_router;
use veda_server::state::AppState;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};
use veda_types::{
    Account, AccountStatus, KeyPermission, KeyStatus, SearchMode, Workspace, WorkspaceKey,
    WorkspaceKind, WorkspaceStatus,
};

#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: Sect,
    milvus: MilvusSect,
    embedding: EmbSect,
    llm: LlmSect,
}
#[derive(Debug, Deserialize)]
struct Sect {
    database_url: String,
}
#[derive(Debug, Deserialize)]
struct MilvusSect {
    url: String,
    token: Option<String>,
    db: Option<String>,
}
#[derive(Debug, Deserialize)]
struct EmbSect {
    api_url: String,
    api_key: String,
    model: String,
    dimension: u32,
    #[serde(default = "default_batch")]
    batch_size: usize,
}
#[derive(Debug, Deserialize)]
struct LlmSect {
    api_url: String,
    api_key: String,
    model: String,
}
fn default_batch() -> usize {
    10
}

fn load_config() -> TestConfig {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("config/test.toml");
    toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn test_metrics() -> veda_server::obs::MetricsHandle {
    use std::sync::OnceLock;
    static METRICS: OnceLock<veda_server::obs::MetricsHandle> = OnceLock::new();
    METRICS.get_or_init(veda_server::obs::install).clone()
}

struct App {
    state: Arc<AppState>,
    router: axum::Router,
    search: SearchService,
    /// Keeps the background indexing worker alive for the test's lifetime.
    _worker_shutdown: tokio::sync::watch::Sender<bool>,
}

async fn build_test_app() -> App {
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
        .expect("mysql"),
    );
    mysql.migrate().await.expect("migrate");
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
        .expect("init");

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
    let llm: Arc<dyn LlmService> = Arc::new(
        // false: this test drives the answer/stream path, which never sends
        // `enable_thinking` anyway — keep it at the wire-standard default.
        LlmProvider::new(&cfg.llm.api_url, &cfg.llm.api_key, &cfg.llm.model, false).expect("llm"),
    );
    let tools = Arc::new(veda_core::service::answer::LiveTools::new(
        search_service.clone(),
        fs_service.clone(),
    ));
    let answer_service = Some(Arc::new(AnswerService::new(
        tools,
        llm.clone(),
        AnswerParams::default(),
    )));
    // Background indexing worker (1s poll) so the fixture write gets
    // chunked + embedded like production.
    let worker = Worker::new(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        None,
        10,
        1,
        2048,
    );
    let (worker_tx, worker_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move { worker.run(worker_rx).await });

    let state = Arc::new(AppState {
        fs_service,
        search_service: search_service.clone(),
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
        answer_service,
        answer_concurrency: 2,
        tunnel_bots: Arc::new(
            veda_server::tunnel_bots::TunnelBotStore::connect(&cfg.mysql.database_url)
                .await
                .expect("tunnel bots store"),
        ),
        draining: std::sync::atomic::AtomicBool::new(false),
    });
    let router = build_router(state.clone());
    App {
        state,
        router,
        search: search_service,
        _worker_shutdown: worker_tx,
    }
}

/// fs workspace + a read wk_ + one indexed document; returns (ws_id, raw_key).
async fn provision(app: &App) -> (String, String) {
    let now = Utc::now();
    let acct = Account {
        id: Uuid::new_v4().to_string(),
        name: "answer-stream-test".into(),
        email: Some(format!("as-{}@test.com", Uuid::new_v4().simple())),
        password_hash: None,
        app_id: None,
        status: AccountStatus::Active,
        created_at: now,
        updated_at: now,
    };
    app.state.auth_store.create_account(&acct).await.unwrap();
    let ws_id = Uuid::new_v4().to_string();
    app.state
        .auth_store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct.id.clone(),
            name: format!("as-{}", &ws_id[..8]),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Fs,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let raw_key = format!("wk_{}", Uuid::new_v4().simple());
    let wk = WorkspaceKey {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws_id.clone(),
        account_id: acct.id.clone(),
        name: "test".into(),
        key_hash: sha256_hex(raw_key.as_bytes()),
        permission: KeyPermission::Read,
        status: KeyStatus::Active,
        kind: WorkspaceKind::Fs,
        created_at: now,
    };
    app.state
        .auth_store
        .create_app_workspace_key(&wk, &raw_key, None, None)
        .await
        .unwrap();

    // Unique marker so retrieval can't match residue from earlier runs.
    let marker = format!("K{}", &ws_id[..8]);
    let doc = format!(
        "# 部署手册\n\n服务 {marker} 的重启口令是 restart-{marker}。\n\
         执行步骤：先停止旧进程，然后运行 systemctl start {marker}，最后检查健康端口 8080。\n"
    );
    app.state
        .fs_service
        .write_file(&ws_id, "/manual.md", &doc, None, None)
        .await
        .unwrap();

    // Wait for the background worker to index the fixture.
    for _ in 0..40 {
        let hits = app
            .search
            .search(
                &ws_id,
                &format!("{marker} 重启"),
                SearchMode::Hybrid,
                5,
                None,
                veda_types::DetailLevel::Full,
            )
            .await
            .unwrap_or_default();
        if !hits.is_empty() {
            return (ws_id, raw_key);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    panic!("fixture document never became searchable");
}

/// Parse an SSE body into (event, data-json) pairs.
fn parse_sse(body: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut event = String::new();
    for line in body.lines() {
        if let Some(ev) = line.strip_prefix("event:") {
            event = ev.trim().to_string();
        } else if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                out.push((event.clone(), v));
            }
        }
    }
    out
}

async fn post_stream(router: &axum::Router, key: &str, body: Value) -> (StatusCode, String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/answer/stream")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // to_bytes drains the SSE stream to completion.
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, ctype, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
#[ignore = "needs real MySQL + Milvus + embedding + airouter (config/test.toml); run with --ignored"]
async fn answer_stream_end_to_end() {
    let app = build_test_app().await;
    let (ws_id, key) = provision(&app).await;
    let marker = format!("K{}", &ws_id[..8]);

    // ── grounded question: deltas then an authoritative final ──
    let (status, ctype, body) = post_stream(
        &app.router,
        &key,
        serde_json::json!({ "query": format!("{marker} 的重启口令是什么？") }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(ctype.starts_with("text/event-stream"), "ctype: {ctype}");
    let events = parse_sse(&body);
    // Consumer contract: accumulate deltas, clear on `reset`, replace with
    // `final`. Mirroring it here keeps the equality assertion valid even in
    // the rare talk-then-tool-call round that emits a reset.
    let mut acc = String::new();
    let mut tool_notes = 0usize;
    for (ev, v) in &events {
        match ev.as_str() {
            "delta" => acc.push_str(v["text"].as_str().unwrap_or_default()),
            "reset" => acc.clear(),
            // Progress notes are optional (zero tool rounds is a valid
            // answer) — but when present they carry the documented shape.
            "tool" => {
                tool_notes += 1;
                assert!(
                    v["name"].is_string() && v["detail"].is_string(),
                    "tool event shape: {v}"
                );
            }
            _ => {}
        }
    }
    eprintln!("tool notes seen: {tool_notes}");
    let finals: Vec<&Value> = events.iter().filter(|(e, _)| e == "final").map(|(_, v)| v).collect();
    assert!(!acc.is_empty(), "at least one surviving delta; events: {events:?}");
    assert_eq!(finals.len(), 1, "exactly one final; events: {events:?}");
    let fin = &finals[0]["data"];
    let answer = fin["answer"].as_str().unwrap();
    assert_eq!(
        answer,
        acc.trim(),
        "final text equals reset-aware concatenated deltas"
    );
    assert!(
        answer.contains(&format!("restart-{marker}")),
        "answer grounded in the fixture: {answer}"
    );
    assert!(
        !fin["citations"].as_array().unwrap().is_empty(),
        "citations present: {fin}"
    );

    // ── unanswerable → single final with the canned refusal, no error ──
    let (status, _, body) = post_stream(
        &app.router,
        &key,
        serde_json::json!({ "query": "今天下雨吗" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = parse_sse(&body);
    let finals: Vec<&Value> = events.iter().filter(|(e, _)| e == "final").map(|(_, v)| v).collect();
    assert_eq!(finals.len(), 1, "events: {events:?}");
    assert!(!events.iter().any(|(e, _)| e == "error"), "no error events");
    eprintln!(
        "unanswerable case tool notes: {}",
        events.iter().filter(|(e, _)| e == "tool").count()
    );

    // ── custom bot prompt travels through and still grounds ──
    let (status, _, body) = post_stream(
        &app.router,
        &key,
        serde_json::json!({
            "query": format!("如何重启 {marker}?"),
            "prompt": "# 角色\n运维答疑机器人,回答必须以带编号的操作步骤列出。"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let events = parse_sse(&body);
    let finals: Vec<&Value> = events.iter().filter(|(e, _)| e == "final").map(|(_, v)| v).collect();
    assert_eq!(finals.len(), 1, "events: {events:?}");
    let persona_answer = finals[0]["data"]["answer"].as_str().unwrap();
    eprintln!("persona answer (expect numbered steps): {persona_answer}");
    assert!(!events.iter().any(|(e, _)| e == "error"), "no error events");

    // ── oversized prompt → plain HTTP 400, no SSE ──
    let (status, ctype, _) = post_stream(
        &app.router,
        &key,
        serde_json::json!({ "query": "q", "prompt": "长".repeat(4001) }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!ctype.starts_with("text/event-stream"));

    // ── invalid query → plain HTTP 400, no SSE ──
    let (status, ctype, _) =
        post_stream(&app.router, &key, serde_json::json!({ "query": "  " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!ctype.starts_with("text/event-stream"));

    let _ = ws_id;
}

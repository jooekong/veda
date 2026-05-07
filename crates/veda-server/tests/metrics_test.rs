//! Metrics integration test. Run with:
//!   cargo test -p veda-server --test metrics_test -- --ignored
//!
//! Calls `obs::install()` at the start (once per binary, which is fine since
//! this file has a single test) then exercises a complete write → worker
//! drain → reconciler pass against real MySQL + Milvus + embedding, and
//! asserts the Prometheus render contains the expected metric names.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{AuthStore, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_server::obs;
use veda_server::reconciler::Reconciler;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore};
use veda_types::{Workspace, WorkspaceStatus};

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
}
#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: MysqlSection,
    milvus: MilvusSection,
    embedding: EmbeddingSection,
}

fn load_config() -> TestConfig {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("config/test.toml");
    let raw = std::fs::read_to_string(&path).expect("read config/test.toml");
    toml::from_str(&raw).expect("parse test.toml")
}

#[tokio::test]
#[ignore]
async fn metrics_render_contains_expected_series() {
    let _ = tracing_subscriber::fmt::try_init();
    let metrics = obs::install();

    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let milvus = Arc::new(MilvusStore::new(
        &cfg.milvus.url,
        cfg.milvus.token.clone(),
        cfg.milvus.db.clone(),
    ));
    milvus
        .init_collections(cfg.embedding.dimension)
        .await
        .unwrap();
    let embedding = Arc::new(
        EmbeddingProvider::new(
            &cfg.embedding.api_url,
            &cfg.embedding.api_key,
            &cfg.embedding.model,
            Some(cfg.embedding.dimension),
            100,
        )
        .unwrap(),
    );
    let fs = FsService::new(mysql.clone());
    let ws = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    mysql
        .create_workspace(&Workspace {
            id: ws.clone(),
            account_id: Uuid::new_v4().to_string(),
            name: format!("metrics-{}", &ws[..8]),
            status: WorkspaceStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    // Emit something for every metric we care about:
    //   * fs.write_file → outbox row created, eventually picked up by worker
    //   * worker.process_task → veda_outbox_process_seconds + veda_embed_*
    //   * reconciler.reconcile_workspace → veda_drift_total gauges
    fs.write_file(&ws, "/m.md", "metrics test content", None, None)
        .await
        .unwrap();

    let worker = Worker::new(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        None,
        4,
        1,
        2048,
    );
    let (tx, rx) = watch::channel(false);
    let h = tokio::spawn(async move { worker.run(rx).await });
    tokio::time::sleep(Duration::from_secs(8)).await;
    let _ = tx.send(true);
    let _ = h.await;

    let recon = Reconciler::with_grace_passes(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        60,
        0,
    );
    // Use run_once, not reconcile_workspace: the cluster-wide drift gauges
    // are only emitted after the per-pass aggregation in run_once. Single-
    // workspace reconciliation does the work but doesn't update metrics.
    let _ = recon.run_once().await.unwrap();

    let body = metrics.render();
    eprintln!("--- BEGIN /metrics render ---\n{body}\n--- END ---");

    // Outbox processing histogram + counter must appear after the worker ran.
    assert!(
        body.contains("veda_outbox_process_seconds"),
        "missing veda_outbox_process_seconds in render"
    );
    // Embedding histogram fires from the actual embedding API call.
    assert!(
        body.contains("veda_embed_latency_seconds"),
        "missing veda_embed_latency_seconds"
    );
    assert!(
        body.contains("veda_embed_total"),
        "missing veda_embed_total counter"
    );
    // Drift gauges from reconciler — exposed regardless of drift count.
    assert!(
        body.contains("veda_drift_total"),
        "missing veda_drift_total gauge"
    );

    // Cleanup
    let pool = mysql.pool();
    let _ = sqlx::query("DELETE FROM veda_outbox WHERE workspace_id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_files WHERE workspace_id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_dentries WHERE workspace_id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
}

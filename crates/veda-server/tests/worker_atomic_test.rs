//! Worker atomicity / dedupe integration tests.
//! Run with: `cargo test -p veda-server --test worker_atomic_test -- --ignored`
//!
//! Requires real MySQL + Milvus + embedding service (see config/test.toml).
//! LLM is optional — tests pass without [llm] but skip the summary path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{MetadataStore, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore};

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

async fn cleanup_workspace(mysql: &MysqlStore, milvus: &MilvusStore, ws: &str) {
    let pool = mysql.pool();
    // chunks → contents → files → summaries → outbox → fs_events → dentries
    let _ = sqlx::query(
        r#"DELETE fc FROM veda_file_chunks fc
           INNER JOIN veda_files f ON fc.file_id = f.id
           WHERE f.workspace_id = ?"#,
    )
    .bind(ws)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        r#"DELETE c FROM veda_file_contents c
           INNER JOIN veda_files f ON c.file_id = f.id
           WHERE f.workspace_id = ?"#,
    )
    .bind(ws)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM veda_summaries WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_files WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_dentries WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_outbox WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_fs_events WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    // Best-effort Milvus cleanup. delete_chunks needs a file_id; we don't
    // know it after dropping the rows, so leave Milvus as-is — reconciler
    // will clean orphans in Week 2. For test isolation each run uses a
    // unique workspace_id, so no cross-test interference.
    let _ = milvus;
}

async fn build_runtime() -> (
    Arc<MysqlStore>,
    Arc<MilvusStore>,
    Arc<EmbeddingProvider>,
    FsService,
) {
    let cfg = load_config();
    let mysql = Arc::new(
        MysqlStore::new(&cfg.mysql.database_url)
            .await
            .expect("connect mysql"),
    );
    mysql.migrate().await.expect("migrate");
    let milvus = Arc::new(MilvusStore::new(
        &cfg.milvus.url,
        cfg.milvus.token.clone(),
        cfg.milvus.db.clone(),
    ));
    milvus
        .init_collections(cfg.embedding.dimension)
        .await
        .expect("init milvus collections");

    let embedding = Arc::new(
        EmbeddingProvider::new(
            &cfg.embedding.api_url,
            &cfg.embedding.api_key,
            &cfg.embedding.model,
            Some(cfg.embedding.dimension),
        )
        .expect("embedding provider"),
    );

    let fs = FsService::new(mysql.clone());
    (mysql, milvus, embedding, fs)
}

/// Spin a Worker for a short duration, draining the outbox.
/// Returns when shutdown signal is sent.
async fn drain_outbox(
    mysql: Arc<MysqlStore>,
    milvus: Arc<MilvusStore>,
    embedding: Arc<EmbeddingProvider>,
    timeout: Duration,
) {
    let worker = Worker::new(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        None,         // no LLM — tests don't exercise summary path
        4,            // small batch
        1,            // 1s poll
        2048,         // unused without LLM
    );
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        worker.run(rx).await;
    });
    tokio::time::sleep(timeout).await;
    let _ = tx.send(true);
    let _ = handle.await;
}

// ── Tests ──────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn worker_writes_content_hash_after_embed() {
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();

    let resp = fs
        .write_file(&ws, "/a.md", "hello world", None, None)
        .await
        .expect("write_file");
    let file_id = resp.file_id.clone();

    // Pre-condition: hash watermark NULL.
    let f0 = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(f0.last_embedded_content_hash, None);

    // Drain outbox (let worker pick up + embed).
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    // Post-condition: hash watermark equals checksum.
    let f1 = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(
        f1.last_embedded_content_hash.as_deref(),
        Some(f1.checksum_sha256.as_str()),
        "watermark must match checksum after embed"
    );

    cleanup_workspace(&mysql, &milvus, &ws).await;
}

#[tokio::test]
#[ignore]
async fn worker_skips_embed_on_unchanged_content_hash() {
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();

    // Initial write + drain so watermark is set.
    let resp = fs
        .write_file(&ws, "/a.md", "stable content v1", None, None)
        .await
        .expect("write_file");
    let file_id = resp.file_id.clone();
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;
    let f1 = mysql.get_file(&file_id).await.unwrap().unwrap();
    let original_revision = f1.revision;
    let original_hash = f1.last_embedded_content_hash.clone();
    assert!(original_hash.is_some(), "first embed must set watermark");

    // Force a fresh ChunkSync into outbox WITHOUT changing content.
    // The dedup guard would block the regular write path, so we insert
    // an outbox event manually to simulate a stray re-sync (e.g. from
    // a future reconciler trigger).
    let now = chrono::Utc::now();
    sqlx::query(
        r#"INSERT INTO veda_outbox
           (workspace_id, event_type, payload, status, retry_count, max_retries,
            available_at, lease_until, created_at)
           VALUES (?, 'chunk_sync', CAST(? AS JSON), 'pending', 0, 5, ?, NULL, ?)"#,
    )
    .bind(&ws)
    .bind(serde_json::json!({"file_id": file_id}).to_string())
    .bind(now.naive_utc())
    .bind(now.naive_utc())
    .execute(mysql.pool())
    .await
    .expect("manual outbox insert");

    // Drain again — worker should short-circuit on hash match without re-embedding.
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    let f2 = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(
        f2.revision, original_revision,
        "revision must not change (no rewrite)"
    );
    assert_eq!(
        f2.last_embedded_content_hash, original_hash,
        "watermark must remain stable"
    );
    // Verify outbox task got completed (status=completed) without producing a new one.
    let row: Option<(String,)> = sqlx::query_as(
        r#"SELECT status FROM veda_outbox WHERE workspace_id = ? AND event_type = 'chunk_sync' ORDER BY id DESC LIMIT 1"#,
    )
    .bind(&ws)
    .fetch_optional(mysql.pool())
    .await
    .unwrap();
    assert_eq!(row.map(|r| r.0).as_deref(), Some("completed"));

    cleanup_workspace(&mysql, &milvus, &ws).await;
}

#[tokio::test]
#[ignore]
async fn worker_picks_up_new_write_after_in_flight_completes() {
    // Codex adversarial-review regression. A processing event must NOT
    // suppress a subsequent write — otherwise the new content is silently
    // dropped because the worker is committed to embedding the old snapshot.
    //
    // We don't try to time the real worker; we directly seed an in-flight
    // outbox row, write new content (which must enqueue a fresh pending),
    // then drain — and assert the watermark ends up matching the LATEST
    // checksum.
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();

    // First write + drain so the file exists with a real file_id and the
    // happy-path embed runs once.
    let resp_v1 = fs
        .write_file(&ws, "/codex.md", "old content v1", None, None)
        .await
        .expect("write v1");
    let file_id = resp_v1.file_id.clone();
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    // Force the outbox into a fake "in-flight processing" state: insert a
    // ChunkSync(file_id) row directly with status=processing. This simulates
    // a worker that has claimed the task but not yet completed it.
    let now = chrono::Utc::now();
    sqlx::query(
        r#"INSERT INTO veda_outbox
           (workspace_id, event_type, payload, status, retry_count, max_retries,
            available_at, lease_until, created_at)
           VALUES (?, 'chunk_sync', CAST(? AS JSON), 'processing', 0, 5, ?, ?, ?)"#,
    )
    .bind(&ws)
    .bind(serde_json::json!({"file_id": file_id}).to_string())
    .bind(now.naive_utc())
    .bind((now + chrono::Duration::minutes(10)).naive_utc())
    .bind(now.naive_utc())
    .execute(mysql.pool())
    .await
    .expect("seed processing row");

    // User writes a new version while the (fake) worker is still busy.
    // With the bug (dedup against processing), this write would NOT enqueue
    // a new ChunkSync — fixed behavior must enqueue a pending row.
    fs.write_file(&ws, "/codex.md", "new content v2", None, None)
        .await
        .expect("write v2");
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM veda_outbox
         WHERE workspace_id = ? AND event_type = 'chunk_sync'
           AND status = 'pending'
           AND JSON_UNQUOTE(JSON_EXTRACT(payload, '$.file_id')) = ?",
    )
    .bind(&ws)
    .bind(&file_id)
    .fetch_one(mysql.pool())
    .await
    .unwrap();
    assert_eq!(
        pending_count, 1,
        "fresh write during in-flight processing must enqueue a new pending event"
    );

    // Now finish the fake "in-flight" task to unblock claim, and drain.
    sqlx::query(
        "UPDATE veda_outbox SET status = 'completed', lease_until = NULL
         WHERE workspace_id = ? AND status = 'processing'",
    )
    .bind(&ws)
    .execute(mysql.pool())
    .await
    .unwrap();
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    // Assert watermark matches the latest checksum (v2).
    let f = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(
        f.last_embedded_content_hash.as_deref(),
        Some(f.checksum_sha256.as_str()),
        "watermark must reflect the latest content after worker drains"
    );

    cleanup_workspace(&mysql, &milvus, &ws).await;
}

#[tokio::test]
#[ignore]
async fn worker_dedupe_pending_chunksync_through_fs_service() {
    // 5 rapid writes (each different content) → should leave at most 1
    // pending ChunkSync at any moment, because the dedupe guard catches
    // events whose worker hasn't completed yet.
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, _embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();

    for i in 0..5 {
        fs.write_file(&ws, "/rapid.md", &format!("v{i}"), None, None)
            .await
            .expect("write");
    }

    // Dedupe semantics: at any moment there must be at most ONE pending
    // ChunkSync per (workspace_id, file_id). This is the invariant the
    // dedupe path enforces. We do NOT check `processing` because a real
    // worker (or another test process sharing the same MySQL) may have
    // already claimed an event by the time we look — that is fine and
    // expected, the new write would correctly enqueue another pending
    // (covered by `worker_picks_up_new_write_after_in_flight_completes`).
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM veda_outbox
         WHERE workspace_id = ? AND event_type = 'chunk_sync'
           AND status = 'pending'",
    )
    .bind(&ws)
    .fetch_one(mysql.pool())
    .await
    .unwrap();
    assert!(
        pending_count <= 1,
        "5 rapid writes must leave at most 1 pending ChunkSync (got {pending_count})"
    );

    cleanup_workspace(&mysql, &milvus, &ws).await;
}

//! Reconciler integration tests.
//! Run with: `cargo test -p veda-server --test reconciler_test -- --ignored`
//!
//! Requires real MySQL + Milvus + embedding service (see config/test.toml).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{watch, Mutex, MutexGuard};
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{AuthStore, MetadataStore, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_server::reconciler::Reconciler;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore};
use veda_types::{Workspace, WorkspaceStatus};

/// Serialize all reconciler IT tests within this binary. They share the test
/// MySQL+Milvus instance with each other AND with whatever external Veda
/// instance is also pointed at the same database. We can't fix the external
/// case here, but holding this lock at least eliminates *intra-suite*
/// interference (one test's grace-period workspace getting clobbered by
/// another test's grace=0 reconciler iterating all workspaces, etc.).
static TEST_SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

async fn serial_guard() -> MutexGuard<'static, ()> {
    TEST_SERIAL.get_or_init(|| Mutex::new(())).lock().await
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

async fn cleanup_workspace(mysql: &MysqlStore, ws: &str) {
    let pool = mysql.pool();
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
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE id = ?")
        .bind(ws)
        .execute(pool)
        .await;
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
            100,
        )
        .expect("embedding provider"),
    );
    let fs = FsService::new(mysql.clone());
    (mysql, milvus, embedding, fs)
}

async fn create_workspace(mysql: &MysqlStore, ws: &str) {
    // The reconciler discovers workspaces through AuthStore::list_active_workspace_ids,
    // which only returns rows from `veda_workspaces`. Insert one with a stable
    // dummy account_id. We don't need the account row to exist for these tests
    // because the reconciler never joins back to accounts.
    let now = chrono::Utc::now();
    mysql
        .create_workspace(&Workspace {
            id: ws.to_string(),
            account_id: Uuid::new_v4().to_string(),
            name: format!("recon-test-{}", &ws[..8]),
            status: WorkspaceStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("create_workspace");
}

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
        None,
        4,
        1,
        2048,
    );
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move { worker.run(rx).await });
    tokio::time::sleep(timeout).await;
    let _ = tx.send(true);
    let _ = handle.await;
}

/// Reconciler with grace_passes=0 — orphans are deleted on first observation.
/// Use this when the test needs deterministic single-pass behavior. The
/// race-protection grace is exercised separately by `*_grace_period` tests.
fn make_reconciler_immediate(mysql: Arc<MysqlStore>, milvus: Arc<MilvusStore>) -> Reconciler {
    Reconciler::with_grace_passes(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        60,
        0,
    )
}

/// Reconciler with grace_passes=1 — matches production. First pass records
/// orphans, second pass deletes. Used to test that a freshly-written file
/// caught between MySQL/Milvus reads is NOT misclassified as orphan.
fn make_reconciler_with_grace(mysql: Arc<MysqlStore>, milvus: Arc<MilvusStore>) -> Reconciler {
    Reconciler::with_grace_passes(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        60,
        1,
    )
}

// ── Tests ──────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn reconciler_clears_orphan_milvus_chunks() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    // Write + drain so Milvus has chunks for some file_id.
    let resp = fs
        .write_file(&ws, "/orphan.md", "content for orphan test", None, None)
        .await
        .expect("write");
    let file_id = resp.file_id.clone();
    drain_outbox(mysql.clone(), milvus.clone(), embedding.clone(), Duration::from_secs(8)).await;

    // Pre-condition: Milvus knows about this file_id.
    let chunks_before = milvus.list_chunk_file_ids(&ws).await.unwrap();
    assert!(
        chunks_before.contains(&file_id),
        "Milvus must have chunks for file_id before drift induction"
    );

    // Drift induction: drop the dentry + file rows directly, leaving Milvus
    // with stale chunks for a file MySQL no longer knows about.
    sqlx::query("DELETE FROM veda_dentries WHERE workspace_id = ?")
        .bind(&ws)
        .execute(mysql.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM veda_files WHERE id = ?")
        .bind(&file_id)
        .execute(mysql.pool())
        .await
        .unwrap();

    // Run reconciler. The orphan should be detected and chunks deleted.
    let report = make_reconciler_immediate(mysql.clone(), milvus.clone())
        .reconcile_workspace(&ws)
        .await
        .expect("reconcile");

    let ws_report = &report;
    assert!(
        ws_report.chunk_orphan >= 1,
        "expected at least 1 orphan, got {ws_report:?}"
    );

    // Post-condition: Milvus no longer has chunks for the dropped file_id.
    let chunks_after = milvus.list_chunk_file_ids(&ws).await.unwrap();
    assert!(
        !chunks_after.contains(&file_id),
        "orphan chunks must be deleted from Milvus"
    );

    cleanup_workspace(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn reconciler_reembeds_missing_chunks() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    // Write + drain so we have chunks in Milvus.
    let resp = fs
        .write_file(&ws, "/missing.md", "content for missing test", None, None)
        .await
        .expect("write");
    let file_id = resp.file_id.clone();
    drain_outbox(mysql.clone(), milvus.clone(), embedding.clone(), Duration::from_secs(8)).await;

    // Drift induction: delete chunks from Milvus directly. MySQL still knows
    // about file_id AND the watermark still says "already embedded" (because
    // the previous embed actually succeeded — Milvus loss happened later).
    // This is the production scenario: reconciler must force a re-embed via
    // payload flag, not rely on the worker's W1.3 hash skip behaving
    // correctly here.
    milvus
        .delete_chunks(&ws, &file_id)
        .await
        .expect("delete_chunks for drift induction");

    // Sanity: confirm the watermark is intact (matches the file's checksum).
    // This is the key precondition Codex's regression depends on.
    let f = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(
        f.last_embedded_content_hash.as_deref(),
        Some(f.checksum_sha256.as_str()),
        "watermark must be intact at the start of the regression test"
    );

    // Pre-condition: Milvus has no chunks for this file_id.
    let chunks_before = milvus.list_chunk_file_ids(&ws).await.unwrap();
    assert!(
        !chunks_before.contains(&file_id),
        "Milvus must have no chunks before reconciler runs"
    );

    // Run reconciler. It should enqueue ChunkSync(file_id).
    let report = make_reconciler_immediate(mysql.clone(), milvus.clone())
        .reconcile_workspace(&ws)
        .await
        .expect("reconcile");
    let ws_report = &report;
    assert!(
        ws_report.chunk_missing >= 1,
        "expected at least 1 missing, got {ws_report:?}"
    );

    // Drain so the worker actually processes the enqueued ChunkSync.
    drain_outbox(mysql.clone(), milvus.clone(), embedding.clone(), Duration::from_secs(10)).await;

    // Post-condition: Milvus has the chunks back.
    let chunks_after = milvus.list_chunk_file_ids(&ws).await.unwrap();
    assert!(
        chunks_after.contains(&file_id),
        "reconciler must re-enqueue ChunkSync so worker rebuilds chunks"
    );

    cleanup_workspace(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn reconciler_clean_workspace_reports_zero_drift() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    // Write + drain so the workspace is in a fully-consistent state.
    fs.write_file(&ws, "/clean.md", "consistent content", None, None)
        .await
        .expect("write");
    drain_outbox(mysql.clone(), milvus.clone(), embedding.clone(), Duration::from_secs(8)).await;

    // Reconcile a clean workspace: should report 0 drift.
    let report = make_reconciler_immediate(mysql.clone(), milvus.clone())
        .reconcile_workspace(&ws)
        .await
        .expect("reconcile");
    let ws_report = &report;
    assert_eq!(ws_report.chunk_missing, 0);
    assert_eq!(ws_report.chunk_orphan, 0);

    cleanup_workspace(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn reconciler_skips_disabled_or_archived_workspaces() {
    // Archived workspace must NOT appear in the reconciler's pass: the
    // reconciler relies on AuthStore::list_active_workspace_ids and we
    // filter by `status = 'active'`.
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, _milvus, _embedding, _fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();

    let now = chrono::Utc::now();
    mysql
        .create_workspace(&Workspace {
            id: ws.clone(),
            account_id: Uuid::new_v4().to_string(),
            name: "archived".into(),
            status: WorkspaceStatus::Archived,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("create archived workspace");

    let active = mysql.list_active_workspace_ids().await.unwrap();
    assert!(
        !active.contains(&ws),
        "archived workspace must not be returned"
    );

    // Cleanup
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE id = ?")
        .bind(&ws)
        .execute(mysql.pool())
        .await;
}

/// Codex finding #1 regression: reconciler-driven repair must bypass the
/// W1.3 watermark short-circuit. Set up a file whose Milvus chunks are gone
/// but whose `last_embedded_content_hash` is still equal to its checksum
/// (the "Milvus lost data after a successful embed" production scenario).
/// Without `force_reembed`, the worker would no-op and the data stays lost.
#[tokio::test]
#[ignore]
async fn reconciler_force_reembeds_when_watermark_intact() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    let resp = fs
        .write_file(&ws, "/codex-repro.md", "intact watermark scenario", None, None)
        .await
        .expect("write");
    let file_id = resp.file_id.clone();
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    let f = mysql.get_file(&file_id).await.unwrap().unwrap();
    let original_hash = f
        .last_embedded_content_hash
        .clone()
        .expect("watermark set after embed");
    assert_eq!(original_hash, f.checksum_sha256);

    milvus.delete_chunks(&ws, &file_id).await.unwrap();
    assert!(
        !milvus
            .list_chunk_file_ids(&ws)
            .await
            .unwrap()
            .contains(&file_id),
        "Milvus must be missing the file after delete"
    );

    let report = make_reconciler_immediate(mysql.clone(), milvus.clone())
        .reconcile_workspace(&ws)
        .await
        .expect("reconcile");
    let ws_report = &report;
    assert_eq!(ws_report.chunk_missing, 1);

    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(10),
    )
    .await;

    let chunks_after = milvus.list_chunk_file_ids(&ws).await.unwrap();
    assert!(
        chunks_after.contains(&file_id),
        "force_reembed must rebuild chunks even with intact watermark"
    );

    cleanup_workspace(&mysql, &ws).await;
}

/// Codex finding #2 regression: a directory summary missing in Milvus must
/// be re-enqueued as DirSummarySync. File child summaries do NOT cascade.
#[tokio::test]
#[ignore]
async fn reconciler_reenqueues_missing_dir_summary() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, _embedding, _fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    // Hand-fabricate "dir has Ready summary in MySQL but no entity in Milvus"
    // without going through the LLM (test infra may lack credentials).
    let dentry_id = Uuid::new_v4().to_string();
    let dir_path = "/docs";
    let now = chrono::Utc::now();
    sqlx::query(
        r#"INSERT INTO veda_dentries (id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at)
           VALUES (?, ?, '/', 'docs', ?, NULL, 1, ?, ?)"#,
    )
    .bind(&dentry_id)
    .bind(&ws)
    .bind(dir_path)
    .bind(now.naive_utc())
    .bind(now.naive_utc())
    .execute(mysql.pool())
    .await
    .expect("insert dentry");
    sqlx::query(
        r#"INSERT INTO veda_summaries (id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview, status)
           VALUES (?, ?, NULL, ?, 'l0', 'l1', 'ready')"#,
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&ws)
    .bind(&dentry_id)
    .execute(mysql.pool())
    .await
    .expect("insert summary");

    let summaries_before = milvus.list_summary_ids(&ws).await.unwrap();
    assert!(!summaries_before.contains(&dentry_id));

    let report = make_reconciler_immediate(mysql.clone(), milvus.clone())
        .reconcile_workspace(&ws)
        .await
        .expect("reconcile");
    let ws_report = &report;
    assert_eq!(
        ws_report.summary_missing, 1,
        "missing dir summary must be reported and enqueued"
    );

    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT event_type,
                  CAST(JSON_UNQUOTE(JSON_EXTRACT(payload, '$.dentry_id')) AS CHAR),
                  CAST(JSON_UNQUOTE(JSON_EXTRACT(payload, '$.parent_path')) AS CHAR)
           FROM veda_outbox
           WHERE workspace_id = ? AND event_type = 'dir_summary_sync'
           ORDER BY id DESC LIMIT 1"#,
    )
    .bind(&ws)
    .fetch_optional(mysql.pool())
    .await
    .unwrap();
    let (et, dent, pp) = row.expect("DirSummarySync row exists");
    assert_eq!(et, "dir_summary_sync");
    assert_eq!(dent.as_deref(), Some(dentry_id.as_str()));
    assert_eq!(pp.as_deref(), Some(dir_path));

    cleanup_workspace(&mysql, &ws).await;
}

/// Codex finding #3 regression: with grace_passes=1, an orphan observed
/// for the first time MUST NOT be deleted. This protects against the
/// snapshot race (user writes between MySQL read and Milvus read).
#[tokio::test]
#[ignore]
async fn reconciler_grace_period_defers_first_pass_orphan_delete() {
    let _g = serial_guard().await;
    let _ = tracing_subscriber::fmt::try_init();
    let (mysql, milvus, embedding, fs) = build_runtime().await;
    let ws = Uuid::new_v4().to_string();
    create_workspace(&mysql, &ws).await;

    let resp = fs
        .write_file(&ws, "/grace.md", "for grace test", None, None)
        .await
        .expect("write");
    let file_id = resp.file_id.clone();
    drain_outbox(
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        Duration::from_secs(8),
    )
    .await;

    sqlx::query("DELETE FROM veda_dentries WHERE workspace_id = ?")
        .bind(&ws)
        .execute(mysql.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM veda_files WHERE id = ?")
        .bind(&file_id)
        .execute(mysql.pool())
        .await
        .unwrap();

    let reconciler = make_reconciler_with_grace(mysql.clone(), milvus.clone());
    let report1 = reconciler.reconcile_workspace(&ws).await.expect("pass 1");
    let ws_report1 = &report1;
    assert_eq!(
        ws_report1.chunk_orphan, 0,
        "grace period: first pass must not delete"
    );
    assert!(
        milvus
            .list_chunk_file_ids(&ws)
            .await
            .unwrap()
            .contains(&file_id),
        "first pass must not have deleted Milvus chunks"
    );

    let report2 = reconciler.reconcile_workspace(&ws).await.expect("pass 2");
    let ws_report2 = &report2;
    assert_eq!(ws_report2.chunk_orphan, 1, "second pass must delete");
    assert!(
        !milvus
            .list_chunk_file_ids(&ws)
            .await
            .unwrap()
            .contains(&file_id),
        "second pass must have deleted Milvus chunks"
    );

    cleanup_workspace(&mysql, &ws).await;
}

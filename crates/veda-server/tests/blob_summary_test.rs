//! PDF/Word L0/L1 summaries: the extract → summary handoff. Run with:
//!   NO_PROXY='*' cargo test -p veda-server --test blob_summary_test -- --ignored --test-threads=1
//!
//! Requires real MySQL + Milvus + embedding, and `[llm]` for the second test
//! (see config/test.toml).
//!
//! Guards the fix for "PDF/Word never get a summary": write-time enqueue only
//! produces ExtractSync for blobs, so SummarySync has to be born in
//! `handle_extract_sync` once a text layer exists. These tests cover the
//! wiring; the freshness truth tables behind the two skip guards are unit
//! tests in `src/worker.rs`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::watch;
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{AuthStore, LlmService, MetadataStore, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_pipeline::llm::LlmProvider;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore};
use veda_types::{SourceType, Workspace, WorkspaceKind, WorkspaceStatus};

/// Real 1-page PDF with a text layer — same fixture as blob_extract_test.
const PDF_BYTES: &[u8] = include_bytes!("fixtures/veda_e2e.pdf");

/// Opaque binary: no text layer, never extracted. Present in every test as
/// the control that must stay summary-free — this is the 2026-07-13
/// hardening (315 dead letters in prod) that the fix must not undo.
const JAR_BYTES: &[u8] = b"PK\x03\x04\x00\x01\xff\xfe\0jar\0bytes\xc0here";

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
    #[serde(default = "default_embed_batch")]
    batch_size: usize,
}
fn default_embed_batch() -> usize {
    10
}
#[derive(Debug, Deserialize)]
struct LlmSection {
    api_url: String,
    api_key: String,
    model: String,
    #[serde(default = "default_summary_tokens")]
    max_summary_tokens: usize,
    #[serde(default)]
    summary_disable_thinking: bool,
}
fn default_summary_tokens() -> usize {
    8192
}
#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: MysqlSection,
    milvus: MilvusSection,
    embedding: EmbeddingSection,
    llm: Option<LlmSection>,
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

struct Rt {
    mysql: Arc<MysqlStore>,
    milvus: Arc<MilvusStore>,
    embedding: Arc<EmbeddingProvider>,
    fs: FsService,
}

async fn build_runtime(cfg: &TestConfig) -> Rt {
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
            cfg.embedding.batch_size,
        )
        .unwrap(),
    );
    let fs = FsService::new(mysql.clone());
    Rt {
        mysql,
        milvus,
        embedding,
        fs,
    }
}

async fn make_workspace(mysql: &Arc<MysqlStore>) -> String {
    let ws = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    mysql
        .create_workspace(&Workspace {
            id: ws.clone(),
            account_id: Uuid::new_v4().to_string(),
            name: format!("blobsum-{}", &ws[..8]),
            status: WorkspaceStatus::Active,
            kind: WorkspaceKind::Fs,
            app_id: None,
            description: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    ws
}

/// Outbox rows of `event_type` carrying this `file_id`, in ANY status. The
/// worker claims and completes rows as it goes, so a `status = 'pending'`
/// filter (what `has_pending_event` does) would race with the very handoff
/// under test.
async fn count_outbox(mysql: &MysqlStore, ws: &str, event_type: &str, file_id: &str) -> i64 {
    sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM veda_outbox
           WHERE workspace_id = ? AND event_type = ?
             AND JSON_UNQUOTE(JSON_EXTRACT(payload, '$.file_id')) = ?"#,
    )
    .bind(ws)
    .bind(event_type)
    .bind(file_id)
    .fetch_one(mysql.pool())
    .await
    .unwrap()
}

async fn count_dead(mysql: &MysqlStore, ws: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM veda_outbox WHERE workspace_id = ? AND status = 'dead'")
        .bind(ws)
        .fetch_one(mysql.pool())
        .await
        .unwrap()
}

/// Run the worker until `done` reports true or the deadline passes, then shut
/// it down cleanly. Returns whether `done` ever became true so callers can
/// assert with a useful message rather than on a timed-out side effect.
async fn run_worker_until<F, Fut>(
    rt: &Rt,
    llm: Option<Arc<dyn LlmService>>,
    max_summary_tokens: usize,
    timeout: Duration,
    done: F,
) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let worker = Worker::new(
        rt.mysql.clone(),
        rt.mysql.clone(),
        rt.milvus.clone(),
        rt.embedding.clone(),
        llm,
        4,
        1,
        max_summary_tokens,
    );
    let (tx, rx) = watch::channel(false);
    let h = tokio::spawn(async move { worker.run(rx).await });
    let deadline = std::time::Instant::now() + timeout;
    let mut ok = false;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if done().await {
            ok = true;
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
    }
    let _ = tx.send(true);
    let _ = h.await;
    ok
}

async fn cleanup(mysql: &MysqlStore, milvus: &MilvusStore, ws: &str, file_ids: &[String]) {
    for fid in file_ids {
        let _ = milvus.delete_chunks(ws, fid).await;
        let _ = milvus.delete_summary(ws, fid).await;
    }
    let pool = mysql.pool();
    for stmt in [
        r#"DELETE fb FROM veda_file_blobs fb
           INNER JOIN veda_files f ON fb.file_id = f.id WHERE f.workspace_id = ?"#,
        r#"DELETE fe FROM veda_file_extracts fe
           INNER JOIN veda_files f ON fe.file_id = f.id WHERE f.workspace_id = ?"#,
        r#"DELETE fc FROM veda_file_chunks fc
           INNER JOIN veda_files f ON fc.file_id = f.id WHERE f.workspace_id = ?"#,
        "DELETE FROM veda_summaries WHERE workspace_id = ?",
        "DELETE FROM veda_fs_events WHERE workspace_id = ?",
        "DELETE FROM veda_files WHERE workspace_id = ?",
        "DELETE FROM veda_dentries WHERE workspace_id = ?",
        "DELETE FROM veda_outbox WHERE workspace_id = ?",
        "DELETE FROM veda_workspaces WHERE id = ?",
    ] {
        let _ = sqlx::query(stmt).bind(ws).execute(pool).await;
    }
}

/// The core of the fix: extracting a PDF's text layer enqueues its
/// SummarySync. Needs no LLM — the enqueue happens in `handle_extract_sync`,
/// which is LLM-independent, so this stays runnable without `[llm]`.
///
/// The jar uploaded alongside is the control: opaque binaries get no
/// ExtractSync and must therefore still get no SummarySync, and nothing may
/// dead-letter.
#[tokio::test]
#[ignore]
async fn pdf_extract_enqueues_summary_sync_e2e() {
    let cfg = load_config();
    let rt = build_runtime(&cfg).await;
    let ws = make_workspace(&rt.mysql).await;

    let pdf = rt
        .fs
        .write_blob(&ws, "/doc.pdf", PDF_BYTES.to_vec(), None)
        .await
        .unwrap();
    let jar = rt
        .fs
        .write_blob(&ws, "/app.jar", JAR_BYTES.to_vec(), None)
        .await
        .unwrap();
    assert_eq!(
        rt.mysql.get_file(&pdf.file_id).await.unwrap().unwrap().source_type,
        SourceType::Pdf
    );
    assert_eq!(
        rt.mysql.get_file(&jar.file_id).await.unwrap().unwrap().source_type,
        SourceType::Binary
    );

    // Write time enqueues ExtractSync and nothing else. Asserting this first
    // is what makes the post-worker assertion meaningful: the SummarySync
    // below can only have come from the extract handler.
    assert_eq!(
        count_outbox(&rt.mysql, &ws, "extract_sync", &pdf.file_id).await,
        1,
        "PDF must get an ExtractSync at write time"
    );
    assert_eq!(
        count_outbox(&rt.mysql, &ws, "summary_sync", &pdf.file_id).await,
        0,
        "write path must NOT enqueue SummarySync for a blob (no text layer yet)"
    );

    let mysql = rt.mysql.clone();
    let (w, f) = (ws.clone(), pdf.file_id.clone());
    let converged = run_worker_until(&rt, None, 2048, Duration::from_secs(120), || {
        let (mysql, w, f) = (mysql.clone(), w.clone(), f.clone());
        async move { count_outbox(&mysql, &w, "summary_sync", &f).await > 0 }
    })
    .await;

    assert!(
        converged,
        "ExtractSync must enqueue a SummarySync for the PDF — this is the \
         handoff whose absence left PDF/Word permanently without L0/L1"
    );
    // The extract row is what the queued SummarySync will read; the enqueue
    // is deliberately placed after this upsert so the text is guaranteed
    // present, never a scheduling gamble.
    assert!(
        rt.mysql.get_file_extract(&pdf.file_id).await.unwrap().is_some(),
        "extract row must exist before the summary task can consume it"
    );

    assert_eq!(
        count_outbox(&rt.mysql, &ws, "summary_sync", &jar.file_id).await,
        0,
        "opaque binary must never get a SummarySync"
    );
    assert_eq!(
        count_dead(&rt.mysql, &ws).await,
        0,
        "no dead letters: the blob skip guard must skip, not error"
    );

    cleanup(&rt.mysql, &rt.milvus, &ws, &[pdf.file_id, jar.file_id]).await;
}

/// End to end with a real LLM: a PDF ends up with a non-empty L0/L1 row in
/// `veda_summaries`. Exercises the two guards the first test cannot reach —
/// `handle_summary_sync`'s blob gate must let the PDF through, and
/// `load_full_content` must hand back the extracted text instead of erroring.
///
/// The non-empty assertion is the 2026-07 empty-abstract incident's sentinel:
/// an empty `l0_abstract` written as `status = ready` is exactly that failure
/// shape, so "a row exists" alone is not enough.
#[tokio::test]
#[ignore]
async fn pdf_gets_nonempty_summary_with_real_llm_e2e() {
    let cfg = load_config();
    let Some(llm_cfg) = cfg.llm.as_ref() else {
        panic!("this test needs [llm] in config/test.toml");
    };
    let rt = build_runtime(&cfg).await;
    let ws = make_workspace(&rt.mysql).await;

    let pdf = rt
        .fs
        .write_blob(&ws, "/doc.pdf", PDF_BYTES.to_vec(), None)
        .await
        .unwrap();

    let llm: Arc<dyn LlmService> = Arc::new(
        LlmProvider::new(
            &llm_cfg.api_url,
            &llm_cfg.api_key,
            &llm_cfg.model,
            llm_cfg.summary_disable_thinking,
        )
        .expect("llm"),
    );

    let mysql = rt.mysql.clone();
    let f = pdf.file_id.clone();
    // Two LLM calls (L0 + L1) plus embedding after the extract — allow more
    // headroom than the extract-only test.
    let converged = run_worker_until(
        &rt,
        Some(llm),
        llm_cfg.max_summary_tokens,
        Duration::from_secs(240),
        || {
            let (mysql, f) = (mysql.clone(), f.clone());
            async move { mysql.get_summary_by_file(&f).await.unwrap().is_some() }
        },
    )
    .await;

    assert!(
        converged,
        "PDF must end up with a summary row after extract → summary"
    );
    let s = rt
        .mysql
        .get_summary_by_file(&pdf.file_id)
        .await
        .unwrap()
        .expect("summary row");
    assert!(
        !s.l0_abstract.trim().is_empty(),
        "L0 must not be empty (2026-07 empty-abstract incident shape)"
    );
    assert!(
        !s.l1_overview.trim().is_empty(),
        "L1 must not be empty (2026-07 empty-abstract incident shape)"
    );
    assert_eq!(
        count_dead(&rt.mysql, &ws).await,
        0,
        "summary generation must not dead-letter"
    );

    cleanup(&rt.mysql, &rt.milvus, &ws, &[pdf.file_id]).await;
}

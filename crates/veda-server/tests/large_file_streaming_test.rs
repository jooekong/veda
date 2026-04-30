//! W4 large-file streaming integration tests. Run with:
//!   cargo test -p veda-server --test large_file_streaming_test -- --ignored
//!
//! Requires real MySQL + Milvus + embedding (see config/test.toml).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{watch, Mutex};
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{AuthStore, MetadataStore, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_server::worker::Worker;
use veda_store::{MilvusStore, MysqlStore};
use veda_types::{Workspace, WorkspaceStatus};

// W4 IT shares MySQL with potentially-running external workers / other tests.
// Serialize so we don't trip over each other's outbox claims.
static TEST_SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
fn serial() -> &'static Mutex<()> {
    TEST_SERIAL.get_or_init(|| Mutex::new(()))
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

async fn make_workspace(mysql: &Arc<MysqlStore>) -> String {
    let ws = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    mysql
        .create_workspace(&Workspace {
            id: ws.clone(),
            account_id: Uuid::new_v4().to_string(),
            name: format!("w4-{}", &ws[..8]),
            status: WorkspaceStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    ws
}

async fn cleanup(mysql: &MysqlStore, ws: &str) {
    let pool = mysql.pool();
    let _ = sqlx::query(
        r#"DELETE fc FROM veda_file_chunks fc
           INNER JOIN veda_files f ON fc.file_id = f.id
           WHERE f.workspace_id = ?"#,
    )
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
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE id = ?")
        .bind(ws)
        .execute(pool)
        .await;
}

// 5 MB content — well past the 256 KB inline threshold so storage is chunked,
// but small enough to keep CI runtime bounded. The exact byte count is encoded
// in the data so we can validate ranges precisely.
fn make_large_text(bytes: usize) -> String {
    // 100-byte lines: 95 ascii letters + 4 digit prefix + '\n'
    let body = "x".repeat(95);
    let mut s = String::with_capacity(bytes);
    let mut i: u64 = 0;
    while s.len() < bytes {
        s.push_str(&format!("{:04}{}\n", i % 10_000, body));
        i += 1;
    }
    s.truncate(bytes);
    s
}

#[tokio::test]
#[ignore]
async fn read_file_range_does_not_load_full_chunked_file() {
    let _g = serial().lock().await;
    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    // 5 MB ~ 20+ chunks at 256 KB each.
    let total_bytes = 5 * 1024 * 1024;
    let content = make_large_text(total_bytes);
    fs.write_file(&ws, "/big.txt", &content, None, None)
        .await
        .unwrap();

    // Verify storage actually went chunked.
    let dentry = mysql.get_dentry(&ws, "/big.txt").await.unwrap().unwrap();
    let file_id = dentry.file_id.unwrap();
    let byte_lens = mysql.list_chunk_byte_lens(&file_id).await.unwrap();
    assert!(
        byte_lens.len() >= 2,
        "expected multiple chunks for 5MB file, got {}",
        byte_lens.len()
    );

    // Read 1 KB from the middle and validate it matches the slice.
    let offset = (total_bytes as u64) / 2;
    let length = 1024;
    let (data, reported_total) = fs
        .read_file_range(&ws, "/big.txt", offset, length)
        .await
        .unwrap();
    assert_eq!(reported_total, total_bytes as u64);
    let expected = &content.as_bytes()[offset as usize..(offset + length) as usize];
    assert_eq!(data.as_slice(), expected);

    // Past EOF must yield empty (W4.2 boundary).
    let (data, _) = fs
        .read_file_range(&ws, "/big.txt", (total_bytes as u64) + 100, 64)
        .await
        .unwrap();
    assert!(data.is_empty(), "past EOF must return empty");

    cleanup(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn worker_streams_chunks_for_large_file_embed() {
    let _g = serial().lock().await;
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
        )
        .unwrap(),
    );

    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    // 2 MB so the test stays under embedding API budget but still exercises
    // the chunk-batch read + EMBED_BATCH split paths.
    let content = make_large_text(2 * 1024 * 1024);
    fs.write_file(&ws, "/big.txt", &content, None, None)
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
    tokio::time::sleep(Duration::from_secs(20)).await;
    let _ = tx.send(true);
    let _ = h.await;

    // After embed, watermark should equal current checksum (worker wrote it).
    let dentry = mysql.get_dentry(&ws, "/big.txt").await.unwrap().unwrap();
    let file_id = dentry.file_id.unwrap();
    let file = mysql.get_file(&file_id).await.unwrap().unwrap();
    assert_eq!(
        file.last_embedded_content_hash.as_deref(),
        Some(file.checksum_sha256.as_str()),
        "watermark must match checksum after streaming embed"
    );

    cleanup(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn get_file_chunks_returns_empty_when_range_exceeds_eof() {
    let _g = serial().lock().await;
    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    // 1 MB → ~4 chunks. The file has ~10_500 lines (100B per line).
    let content = make_large_text(1024 * 1024);
    fs.write_file(&ws, "/eof.txt", &content, None, None)
        .await
        .unwrap();

    let dentry = mysql.get_dentry(&ws, "/eof.txt").await.unwrap().unwrap();
    let file_id = dentry.file_id.unwrap();

    let chunks = mysql
        .get_file_chunks(&file_id, Some(100_000), Some(100_100))
        .await
        .unwrap();
    assert!(
        chunks.is_empty(),
        "querying past EOF must return empty (W4.2 fix), got {} chunks",
        chunks.len()
    );

    cleanup(&mysql, &ws).await;
}

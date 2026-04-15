//! MySQL integration tests. Run with: `cargo test -p veda-store -- --ignored`

use std::path::PathBuf;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;
use veda_core::store::{MetadataStore, TaskQueue};
use veda_store::MysqlStore;
use veda_types::{
    Dentry, FileChunk, FileRecord, OutboxEvent, OutboxEventType, OutboxStatus, SourceType,
    StorageType,
};

#[derive(Debug, Deserialize)]
struct MysqlSection {
    database_url: String,
}

#[derive(Debug, Deserialize)]
struct TestToml {
    mysql: MysqlSection,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn load_mysql_url() -> String {
    let path = workspace_root().join("config/test.toml");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg: TestToml = toml::from_str(&raw).expect("parse test.toml [mysql]");
    cfg.mysql.database_url
}

async fn cleanup_workspace(store: &MysqlStore, workspace_id: &str) {
    let pool = store.pool();
    let _ = sqlx::query(
        r#"DELETE fc FROM veda_file_chunks fc
           INNER JOIN veda_files f ON fc.file_id = f.id
           WHERE f.workspace_id = ?"#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        r#"DELETE c FROM veda_file_contents c
           INNER JOIN veda_files f ON c.file_id = f.id
           WHERE f.workspace_id = ?"#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await;
    let _ = sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ?"#)
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_files WHERE workspace_id = ?"#)
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_outbox WHERE workspace_id = ?"#)
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_fs_events WHERE workspace_id = ?"#)
        .bind(workspace_id)
        .execute(pool)
        .await;
}

fn sample_dentry(ws: &str, path: &str, name: &str, file_id: Option<&str>) -> Dentry {
    let now = Utc::now();
    Dentry {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.to_string(),
        parent_path: "/".to_string(),
        name: name.to_string(),
        path: path.to_string(),
        file_id: file_id.map(String::from),
        is_dir: false,
        created_at: now,
        updated_at: now,
    }
}

fn sample_file(ws: &str, id: &str, checksum: &str) -> FileRecord {
    let now = Utc::now();
    FileRecord {
        id: id.to_string(),
        workspace_id: ws.to_string(),
        size_bytes: 3,
        mime_type: "text/plain".into(),
        storage_type: StorageType::Inline,
        source_type: SourceType::Text,
        line_count: Some(1),
        checksum_sha256: checksum.to_string(),
        revision: 1,
        ref_count: 1,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
#[ignore]
async fn mysql_migrate_and_dentry_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let d = sample_dentry(&ws, "/note.txt", "note.txt", None);
    let mut tx = store.begin_tx().await.expect("begin");
    tx.insert_dentry(&d).await.expect("insert dentry");
    Box::new(tx).commit().await.expect("commit");
    let got = store
        .get_dentry(&ws, "/note.txt")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(got.path, "/note.txt");
    let mut tx = store.begin_tx().await.expect("begin");
    let n = tx.delete_dentry(&ws, "/note.txt").await.expect("del");
    assert_eq!(n, 1);
    Box::new(tx).commit().await.expect("commit");
    cleanup_workspace(&store, &ws).await;
}

#[tokio::test]
#[ignore]
async fn mysql_file_and_content() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let fid = Uuid::new_v4().to_string();
    let file = sample_file(&ws, &fid, "deadbeef");
    let mut tx = store.begin_tx().await.expect("begin");
    tx.insert_file(&file).await.expect("insert file");
    tx.insert_file_content(&fid, "abc").await.expect("content");
    Box::new(tx).commit().await.expect("commit");
    let body = store
        .get_file_content(&fid)
        .await
        .expect("read")
        .expect("some");
    assert_eq!(body, "abc");
    cleanup_workspace(&store, &ws).await;
}

#[tokio::test]
#[ignore]
async fn mysql_tx_atomic_dentry_and_file() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let fid = Uuid::new_v4().to_string();
    let mut d = sample_dentry(&ws, "/f.txt", "f.txt", Some(&fid));
    d.file_id = Some(fid.clone());
    let file = sample_file(&ws, &fid, "cafebabe");
    let mut tx = store.begin_tx().await.expect("begin");
    tx.insert_file(&file).await.expect("file");
    tx.insert_dentry(&d).await.expect("dentry");
    Box::new(tx).commit().await.expect("commit");
    let dentry = store.get_dentry(&ws, "/f.txt").await.unwrap().unwrap();
    assert_eq!(dentry.file_id.as_deref(), Some(fid.as_str()));
    cleanup_workspace(&store, &ws).await;
}

#[tokio::test]
#[ignore]
async fn mysql_task_queue_enqueue_claim_complete() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let ev = OutboxEvent {
        id: 0,
        workspace_id: ws.clone(),
        event_type: OutboxEventType::ChunkSync,
        payload: serde_json::json!({"k": 1}),
        status: OutboxStatus::Pending,
        retry_count: 0,
        max_retries: 5,
        available_at: Utc::now(),
        lease_until: None,
        created_at: Utc::now(),
    };
    store.enqueue(&ev).await.expect("enqueue");
    let claimed = store.claim(10).await.expect("claim");
    let mine = claimed
        .iter()
        .find(|e| e.workspace_id == ws)
        .expect("claimed row for this workspace");
    let id = mine.id;
    store.complete(id).await.expect("complete");
    let claimed2 = store.claim(10).await.expect("claim2");
    assert!(
        !claimed2.iter().any(|e| e.id == id),
        "completed task should not be claimed again"
    );
    cleanup_workspace(&store, &ws).await;
}

#[tokio::test]
#[ignore]
async fn mysql_checksum_lookup_and_delete_file() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let fid = Uuid::new_v4().to_string();
    let sum = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let file = sample_file(&ws, &fid, sum);
    let mut tx = store.begin_tx().await.expect("begin");
    tx.insert_file(&file).await.unwrap();
    tx.insert_file_chunks(&[FileChunk {
        file_id: fid.clone(),
        chunk_index: 0,
        start_line: 1,
        content: "x".into(),
    }])
    .await
    .unwrap();
    Box::new(tx).commit().await.unwrap();
    let found = store
        .find_file_by_checksum(&ws, sum)
        .await
        .unwrap()
        .expect("by checksum");
    assert_eq!(found.id, fid);
    let chunks = store.get_file_chunks(&fid, None, None).await.unwrap();
    assert_eq!(chunks.len(), 1);
    let mut tx = store.begin_tx().await.unwrap();
    tx.delete_file(&fid).await.unwrap();
    Box::new(tx).commit().await.unwrap();
    assert!(store.get_file(&fid).await.unwrap().is_none());
    cleanup_workspace(&store, &ws).await;
}

//! MySQL integration tests. Run with: `cargo test -p veda-store -- --ignored`

use std::path::PathBuf;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;
use veda_core::store::{AuthStore, CollectionMetaStore, MetadataStore, TaskQueue};
use veda_store::MysqlStore;
use veda_types::{
    Account, AccountStatus, ApiKeyRecord, CollectionSchema, CollectionStatus, CollectionType,
    Dentry, FileChunk, FileRecord, KeyPermission, KeyStatus, OutboxEvent, OutboxEventType,
    OutboxStatus, SourceType, StorageType, Workspace, WorkspaceKey, WorkspaceStatus,
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
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
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

async fn cleanup_account(store: &MysqlStore, account_id: &str) {
    let pool = store.pool();
    let _ = sqlx::query(r#"DELETE FROM veda_workspace_keys WHERE workspace_id IN (SELECT id FROM veda_workspaces WHERE account_id = ?)"#)
        .bind(account_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_workspaces WHERE account_id = ?"#)
        .bind(account_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_api_keys WHERE account_id = ?"#)
        .bind(account_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(r#"DELETE FROM veda_accounts WHERE id = ?"#)
        .bind(account_id)
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
async fn mysql_list_dentries_under_root_lists_workspace_entries() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let ws = Uuid::new_v4().to_string();
    let other_ws = Uuid::new_v4().to_string();
    let d1 = sample_dentry(&ws, "/a.txt", "a.txt", None);
    let d2 = sample_dentry(&ws, "/b.txt", "b.txt", None);
    let d3 = sample_dentry(&other_ws, "/z.txt", "z.txt", None);

    let mut tx = store.begin_tx().await.expect("begin");
    tx.insert_dentry(&d1).await.expect("insert d1");
    tx.insert_dentry(&d2).await.expect("insert d2");
    tx.insert_dentry(&d3).await.expect("insert d3");
    Box::new(tx).commit().await.expect("commit");

    let rows = store
        .list_dentries_under(&ws, "/")
        .await
        .expect("list under root");
    let paths: Vec<String> = rows.into_iter().map(|d| d.path).collect();
    assert_eq!(paths, vec!["/a.txt".to_string(), "/b.txt".to_string()]);

    cleanup_workspace(&store, &ws).await;
    cleanup_workspace(&store, &other_ws).await;
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
    // drain stale pending entries from previous test runs
    loop {
        let batch = store.claim(100).await.expect("drain");
        if batch.is_empty() {
            break;
        }
        for e in &batch {
            let _ = store.complete(e.id).await;
        }
    }
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
        line_count: 1,
        byte_len: 1,
        chunk_sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(),
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

// ── Auth CRUD tests ────────────────────────────────────

#[tokio::test]
#[ignore]
async fn mysql_account_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let acct = Account {
        id: acct_id.clone(),
        name: "test-user".into(),
        email: Some(format!("{}@test.com", &acct_id[..8])),
        password_hash: Some("argon2hash".into()),
        status: AccountStatus::Active,
        created_at: now,
        updated_at: now,
    };
    store.create_account(&acct).await.unwrap();

    let got = store.get_account(&acct_id).await.unwrap().unwrap();
    assert_eq!(got.name, "test-user");
    assert_eq!(got.status, AccountStatus::Active);

    let by_email = store
        .get_account_by_email(acct.email.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_email.id, acct_id);

    let missing = store
        .get_account_by_email("nonexistent@x.com")
        .await
        .unwrap();
    assert!(missing.is_none());

    cleanup_account(&store, &acct_id).await;
}

#[tokio::test]
#[ignore]
async fn mysql_api_key_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "key-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let key_id = Uuid::new_v4().to_string();
    let key_hash = "sha256_of_key_abc123";
    let ak = ApiKeyRecord {
        id: key_id.clone(),
        account_id: acct_id.clone(),
        name: "default".into(),
        key_hash: key_hash.into(),
        status: KeyStatus::Active,
        created_at: now,
    };
    store.create_api_key(&ak).await.unwrap();

    let got = store.get_api_key_by_hash(key_hash).await.unwrap().unwrap();
    assert_eq!(got.id, key_id);
    assert_eq!(got.account_id, acct_id);

    let keys = store.list_api_keys(&acct_id).await.unwrap();
    assert_eq!(keys.len(), 1);

    store.revoke_api_key(&key_id).await.unwrap();
    let revoked = store.get_api_key_by_hash(key_hash).await.unwrap();
    assert!(revoked.is_none(), "revoked key should not be found");

    cleanup_account(&store, &acct_id).await;
}

#[tokio::test]
#[ignore]
async fn mysql_workspace_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    let acct_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "ws-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let ws_id = Uuid::new_v4().to_string();
    let ws = Workspace {
        id: ws_id.clone(),
        account_id: acct_id.clone(),
        name: "my-project".into(),
        status: WorkspaceStatus::Active,
        created_at: now,
        updated_at: now,
    };
    store.create_workspace(&ws).await.unwrap();

    let got = store.get_workspace(&ws_id).await.unwrap().unwrap();
    assert_eq!(got.name, "my-project");

    let list = store.list_workspaces(&acct_id).await.unwrap();
    assert_eq!(list.len(), 1);

    store.delete_workspace(&ws_id).await.unwrap();
    let archived = store.get_workspace(&ws_id).await.unwrap().unwrap();
    assert_eq!(archived.status, WorkspaceStatus::Archived);

    let list_after = store.list_workspaces(&acct_id).await.unwrap();
    assert!(
        list_after.is_empty(),
        "archived workspace not in active list"
    );

    cleanup_account(&store, &acct_id).await;
}

#[tokio::test]
#[ignore]
async fn mysql_workspace_key_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    let acct_id = Uuid::new_v4().to_string();
    let ws_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    store
        .create_account(&Account {
            id: acct_id.clone(),
            name: "wk-test".into(),
            email: Some(format!("{}@test.com", &acct_id[..8])),
            password_hash: None,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    store
        .create_workspace(&Workspace {
            id: ws_id.clone(),
            account_id: acct_id.clone(),
            name: "ws-for-keys".into(),
            status: WorkspaceStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let wk_id = Uuid::new_v4().to_string();
    let wk_hash = "sha256_of_workspace_key_xyz";
    let wk = WorkspaceKey {
        id: wk_id.clone(),
        workspace_id: ws_id.clone(),
        name: "ci-key".into(),
        key_hash: wk_hash.into(),
        permission: KeyPermission::ReadWrite,
        status: KeyStatus::Active,
        created_at: now,
    };
    store.create_workspace_key(&wk).await.unwrap();

    let got = store
        .get_workspace_key_by_hash(wk_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.id, wk_id);
    assert_eq!(got.permission, KeyPermission::ReadWrite);

    let keys = store.list_workspace_keys(&ws_id).await.unwrap();
    assert_eq!(keys.len(), 1);

    store.revoke_workspace_key(&wk_id).await.unwrap();
    let revoked = store.get_workspace_key_by_hash(wk_hash).await.unwrap();
    assert!(revoked.is_none());

    cleanup_account(&store, &acct_id).await;
}

// ── Collection Schema tests ────────────────────────────

#[tokio::test]
#[ignore]
async fn mysql_collection_schema_crud() {
    let url = load_mysql_url();
    let store = MysqlStore::new(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    let ws_id = Uuid::new_v4().to_string();
    let coll_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    let schema = CollectionSchema {
        id: coll_id.clone(),
        workspace_id: ws_id.clone(),
        name: "articles".into(),
        collection_type: CollectionType::Structured,
        schema_json: serde_json::json!([
            {"name": "title", "field_type": "string", "index": true, "embed": false},
            {"name": "content", "field_type": "string", "index": false, "embed": true}
        ]),
        embedding_source: Some("content".into()),
        embedding_dim: Some(1024),
        status: CollectionStatus::Active,
        created_at: now,
        updated_at: now,
    };
    store.create_collection_schema(&schema).await.unwrap();

    let got = store
        .get_collection_schema(&ws_id, "articles")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "articles");
    assert_eq!(got.collection_type, CollectionType::Structured);
    assert_eq!(got.embedding_source.as_deref(), Some("content"));

    let by_id = store
        .get_collection_schema_by_id(&coll_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_id.name, "articles");

    let list = store.list_collection_schemas(&ws_id).await.unwrap();
    assert_eq!(list.len(), 1);

    store.delete_collection_schema(&coll_id).await.unwrap();
    let deleted = store
        .get_collection_schema(&ws_id, "articles")
        .await
        .unwrap();
    assert!(deleted.is_none());

    // cleanup
    let pool = store.pool();
    let _ = sqlx::query(r#"DELETE FROM veda_collection_schemas WHERE workspace_id = ?"#)
        .bind(&ws_id)
        .execute(pool)
        .await;
}

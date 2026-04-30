//! Events retention + 410 + path_prefix integration. Run with:
//!   cargo test -p veda-server --test events_retention_test -- --ignored
//!
//! Requires real MySQL (see config/test.toml). Doesn't touch Milvus or
//! the embedding service.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use uuid::Uuid;
use veda_core::service::fs::FsService;
use veda_core::store::{AuthStore, MetadataStore};
use veda_store::MysqlStore;
use veda_types::{Workspace, WorkspaceStatus};

#[derive(Debug, Deserialize)]
struct MysqlSection {
    database_url: String,
}
#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: MysqlSection,
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
            name: format!("ev-{}", &ws[..8]),
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
    let _ = sqlx::query("DELETE FROM veda_fs_events WHERE workspace_id = ?")
        .bind(ws)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM veda_outbox WHERE workspace_id = ?")
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
    let _ = sqlx::query("DELETE FROM veda_workspaces WHERE id = ?")
        .bind(ws)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore]
async fn events_min_id_advances_after_retention_delete() {
    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    fs.write_file(&ws, "/a.txt", "1", None, None).await.unwrap();
    fs.write_file(&ws, "/b.txt", "2", None, None).await.unwrap();
    fs.write_file(&ws, "/c.txt", "3", None, None).await.unwrap();

    let initial_min = fs.events_min_id(&ws).await.unwrap().unwrap();
    let events = fs.query_events(&ws, 0, 100).await.unwrap();
    assert_eq!(events.len(), 3);

    // Simulate retention: delete the first event by created_at filter set to
    // "now" (everything is older). Two events should remain.
    let pool = mysql.pool();
    sqlx::query("UPDATE veda_fs_events SET created_at = ? WHERE workspace_id = ? AND id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::days(30)).naive_utc())
        .bind(&ws)
        .bind(initial_min)
        .execute(pool)
        .await
        .unwrap();

    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let n = fs.prune_events_older_than(cutoff).await.unwrap();
    assert!(n >= 1, "at least one event should be pruned (got {n})");

    let new_min = fs.events_min_id(&ws).await.unwrap().unwrap();
    assert!(
        new_min > initial_min,
        "min_id must advance after retention prune (was {initial_min}, now {new_min})"
    );

    cleanup(&mysql, &ws).await;
}

#[tokio::test]
#[ignore]
async fn events_path_prefix_filters_at_query_layer() {
    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    fs.write_file(&ws, "/docs/a.md", "x", None, None).await.unwrap();
    fs.write_file(&ws, "/src/b.rs", "y", None, None).await.unwrap();
    fs.write_file(&ws, "/docs/c.md", "z", None, None).await.unwrap();

    let docs = fs
        .query_events_filtered(&ws, 0, Some("/docs"), 100)
        .await
        .unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs.iter().all(|e| e.path.starts_with("/docs")));

    // Underscore must not slip through MySQL's LIKE wildcard semantics.
    fs.write_file(&ws, "/docs_alt/d.md", "w", None, None).await.unwrap();
    let docs_only = fs
        .query_events_filtered(&ws, 0, Some("/docs/"), 100)
        .await
        .unwrap();
    assert_eq!(
        docs_only.len(),
        2,
        "/docs/ prefix must not match /docs_alt/* — got {} events",
        docs_only.len()
    );

    cleanup(&mysql, &ws).await;
}

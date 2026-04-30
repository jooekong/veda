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

/// Data-layer simulation of plan §5.8 "three-client collaboration".
/// Does NOT exercise HTTP / SSE — see `build_410_body_carries_*` in
/// `routes/events.rs` for the route-level tests. This IT covers the
/// data flow that *feeds* the 410 decision: cursor-vs-min_id ordering,
/// max_id reporting, and three independent cursors progressing past a
/// retention sweep.
///
/// Roles:
///   Client A — persistent subscriber, sees every event from cursor=0.
///   Client B — subscribes, disconnects, retention prunes B's window,
///              reconnect-with-stale-cursor would yield 410.
///   Client C — persistent subscriber, sees every event in parallel
///              with A.
#[tokio::test]
#[ignore]
async fn three_client_event_flow_data_layer() {
    let cfg = load_config();
    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
    mysql.migrate().await.unwrap();
    let fs = FsService::new(mysql.clone());
    let ws = make_workspace(&mysql).await;

    // ── Phase 1: A and C "subscribe" at cursor=0; B subscribes too. ──
    // A persistent subscriber's behavior is "remember the highest id you've
    // pulled so far" — we model that with three i64 cursors.
    let mut cursor_a: i64 = 0;
    let mut cursor_c: i64 = 0;
    let cursor_b_at_disconnect: i64;

    // Workload writes 5 files. Each `write_file` produces an FsEvent.
    for i in 0..5 {
        fs.write_file(&ws, &format!("/f{i}.txt"), "x", None, None)
            .await
            .unwrap();
    }

    // A and C drain everything from id 0.
    let a_first = fs.query_events(&ws, cursor_a, 100).await.unwrap();
    let c_first = fs.query_events(&ws, cursor_c, 100).await.unwrap();
    assert_eq!(a_first.len(), 5, "A should see all 5 writes");
    assert_eq!(c_first.len(), 5, "C should see all 5 writes");
    cursor_a = a_first.last().unwrap().id;
    cursor_c = c_first.last().unwrap().id;
    assert_eq!(
        cursor_a, cursor_c,
        "A and C must agree on the high water mark"
    );

    // B drains too, then "disconnects" — we just stop polling.
    let b_first = fs.query_events(&ws, 0, 100).await.unwrap();
    cursor_b_at_disconnect = b_first.last().unwrap().id;

    // ── Phase 2: more writes while B is offline. A and C see them. ──
    for i in 5..8 {
        fs.write_file(&ws, &format!("/f{i}.txt"), "y", None, None)
            .await
            .unwrap();
    }
    let a_second = fs.query_events(&ws, cursor_a, 100).await.unwrap();
    let c_second = fs.query_events(&ws, cursor_c, 100).await.unwrap();
    assert_eq!(a_second.len(), 3, "A should see the 3 offline-window writes");
    assert_eq!(c_second.len(), 3, "C should see them too");
    cursor_a = a_second.last().unwrap().id;
    cursor_c = c_second.last().unwrap().id;

    // ── Phase 3: simulate retention sweep — backdate everything ≤ B's
    // cursor and prune. After this, B's saved cursor predates min(id),
    // which is the cue for the SSE route to emit 410. ──
    let pool = mysql.pool();
    sqlx::query(
        "UPDATE veda_fs_events SET created_at = ? WHERE workspace_id = ? AND id <= ?",
    )
    .bind((chrono::Utc::now() - chrono::Duration::days(30)).naive_utc())
    .bind(&ws)
    .bind(cursor_b_at_disconnect)
    .execute(pool)
    .await
    .unwrap();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let _pruned = fs.prune_events_older_than(cutoff).await.unwrap();
    // The previous version compared the global pruned-count against a row id
    // (different units). What we actually care about is "B's window is gone
    // for *this* workspace" — confirm that directly.
    let surviving_below: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM veda_fs_events WHERE workspace_id = ? AND id <= ?",
    )
    .bind(&ws)
    .bind(cursor_b_at_disconnect)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        surviving_below, 0,
        "B's window must be fully pruned in this workspace"
    );

    // ── Phase 4: B "reconnects" with stale cursor. The route's 410 gate
    // checks: cursor_b < min_id → 410. Verify the data layer reports
    // both min_id (above B's cursor) and max_id (head) as the route
    // would surface in the 410 body. ──
    let min_id = fs
        .events_min_id(&ws)
        .await
        .unwrap()
        .expect("workspace still has events post-prune");
    let max_id = fs
        .events_max_id(&ws)
        .await
        .unwrap()
        .expect("workspace still has events post-prune");
    assert!(
        cursor_b_at_disconnect < min_id,
        "B's cursor ({cursor_b_at_disconnect}) must predate min_id ({min_id}) \
         after retention — otherwise the 410 gate wouldn't fire"
    );
    assert!(
        max_id >= min_id,
        "max_id ({max_id}) must not lag min_id ({min_id})"
    );
    // B's recovery protocol: jump to max_id, list_dir resync, resubscribe.
    // Resubscribed B sees no events yet (nothing newer than max_id).
    let b_recovered = fs.query_events(&ws, max_id, 100).await.unwrap();
    assert!(
        b_recovered.is_empty(),
        "B should see no events when resubscribing at max_id"
    );

    // ── Phase 5: more writes after B's recovery. All three cursors should
    // converge — A continues from cursor_a, B from max_id, C from
    // cursor_c. Anyone of them sees the new event(s). ──
    fs.write_file(&ws, "/post_recovery.txt", "z", None, None)
        .await
        .unwrap();
    let a_third = fs.query_events(&ws, cursor_a, 100).await.unwrap();
    let c_third = fs.query_events(&ws, cursor_c, 100).await.unwrap();
    let b_third = fs.query_events(&ws, max_id, 100).await.unwrap();
    assert_eq!(a_third.len(), 1, "A should see the post-recovery write");
    assert_eq!(c_third.len(), 1, "C should see the post-recovery write");
    assert_eq!(b_third.len(), 1, "B should see the post-recovery write");
    let post_id = a_third[0].id;
    assert_eq!(c_third[0].id, post_id);
    assert_eq!(b_third[0].id, post_id);

    cleanup(&mysql, &ws).await;
}

//! Memory store integration tests (docs/plans/agent-memory-m1.md Step 1).
//! Run with: `NO_PROXY='*' cargo test -p veda-store --test memory_test -- --ignored --test-threads=1`

use std::path::PathBuf;

use chrono::{Duration, Utc};
use serde::Deserialize;
use uuid::Uuid;
use veda_core::store::{MemoryStore, MemoryVectorStore, TaskQueue};
use veda_store::{MilvusStore, MysqlStore};
use veda_types::{
    MemoryInsert, MemoryKind, MemoryPatch, MemoryScopeFilter, MemoryScopeType,
    MemoryWithEmbedding, NewMemory, PrincipalKind, PrincipalSource, VedaError,
};

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
    dimension: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TestToml {
    mysql: MysqlSection,
    milvus: MilvusSection,
    embedding: Option<EmbeddingSection>,
}

fn load_config() -> TestToml {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("config/test.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&raw).expect("parse test.toml")
}

fn sha256_hex(s: &str) -> String {
    veda_core::checksum::sha256_hex(s.as_bytes())
}

fn new_memory(
    scope_type: MemoryScopeType,
    scope_id: &str,
    origin: Option<&str>,
    content: &str,
    created_by: &str,
) -> NewMemory {
    NewMemory {
        scope_type,
        scope_id: scope_id.to_string(),
        origin_workspace_id: origin.map(str::to_string),
        topic: Some("testing".to_string()),
        kind: MemoryKind::Fact,
        content: content.to_string(),
        content_hash: sha256_hex(content),
        source_ref: None,
        expires_at: None,
        created_by: created_by.to_string(),
    }
}

#[tokio::test]
#[ignore]
async fn mysql_memory_crud_scoping_and_expiry() {
    let cfg = load_config();
    let store = MysqlStore::new(&cfg.mysql.database_url)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate creates memory tables");

    let ws = format!("wsm_{}", Uuid::new_v4());
    let other_ws = format!("wsm_{}", Uuid::new_v4());

    // Principals: lazy create is idempotent — same (source, external_id)
    // resolves to the same row.
    let key_id = Uuid::new_v4().to_string();
    let p1 = store
        .ensure_principal_for_identity(PrincipalSource::Key, &key_id, PrincipalKind::Human, None)
        .await
        .expect("ensure principal");
    let p2 = store
        .ensure_principal_for_identity(PrincipalSource::Key, &key_id, PrincipalKind::Human, None)
        .await
        .expect("ensure principal again");
    assert_eq!(p1.id, p2.id, "ensure_principal_for_identity must be idempotent");

    let team_scope = (MemoryScopeType::Workspace, ws.clone());
    let personal_scope = (MemoryScopeType::Principal, p1.id.clone());

    // Insert team memory; exact re-insert is a Duplicate of the same row.
    let m1 = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Workspace,
            &ws,
            None,
            "integration tests must run single threaded",
            &p1.id,
        ))
        .await
        .expect("insert m1")
    {
        MemoryInsert::Inserted(m) => m,
        MemoryInsert::Duplicate(_) => panic!("fresh insert reported duplicate"),
    };
    match store
        .insert_memory(&new_memory(
            MemoryScopeType::Workspace,
            &ws,
            None,
            "integration tests must run single threaded",
            &p1.id,
        ))
        .await
        .expect("re-insert m1")
    {
        MemoryInsert::Duplicate(m) => assert_eq!(m.id, m1.id, "duplicate returns existing row"),
        MemoryInsert::Inserted(_) => panic!("unique(scope, hash) did not dedupe"),
    }

    // Personal memories: one project note (origin=ws), one portable
    // (origin None), one belonging to another project.
    let m_note = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Principal,
            &p1.id,
            Some(&ws),
            "this repo needs NO_PROXY for tests",
            &p1.id,
        ))
        .await
        .expect("insert note")
    {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };
    let m_pref = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Principal,
            &p1.id,
            None,
            "joe prefers minimal solutions",
            &p1.id,
        ))
        .await
        .expect("insert pref")
    {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };
    let m_other = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Principal,
            &p1.id,
            Some(&other_ws),
            "other project note",
            &p1.id,
        ))
        .await
        .expect("insert other-project note")
    {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };

    let all_ids = [m1.id, m_note.id, m_pref.id, m_other.id];

    // Context filter (team ws + personal p1 restricted to origin ∈ {ws, none}):
    // the other-project note must NOT come back.
    let ctx = MemoryScopeFilter::Context {
        workspace_id: ws.clone(),
        principal_id: p1.id.clone(),
        dept_id: None,
    };
    let got = store
        .get_memories_by_ids(&all_ids, &ctx)
        .await
        .expect("context read");
    let got_ids: Vec<i64> = got.iter().map(|m| m.id).collect();
    assert!(got_ids.contains(&m1.id), "team memory in context");
    assert!(got_ids.contains(&m_note.id), "project note in context");
    assert!(got_ids.contains(&m_pref.id), "portable pref in context");
    assert!(
        !got_ids.contains(&m_other.id),
        "other-project note must be filtered by origin"
    );

    // Cross-domain leak = 0 at the store level: another principal's scope
    // filter sees none of p1's personal memories.
    let stranger = MemoryScopeFilter::Scope {
        scope_type: MemoryScopeType::Principal,
        scope_id: Uuid::new_v4().to_string(),
    };
    let leaked = store
        .get_memories_by_ids(&all_ids, &stranger)
        .await
        .expect("stranger read");
    assert!(leaked.is_empty(), "cross-domain read must return nothing");

    // Expired rows are filtered SQL-side.
    let mut expired = new_memory(
        MemoryScopeType::Workspace,
        &ws,
        None,
        "already expired fact",
        &p1.id,
    );
    expired.expires_at = Some(Utc::now() - Duration::hours(1));
    let m_exp = match store.insert_memory(&expired).await.expect("insert expired") {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };
    let team_filter = MemoryScopeFilter::Scope {
        scope_type: MemoryScopeType::Workspace,
        scope_id: ws.clone(),
    };
    let live = store
        .get_memories_by_ids(&[m1.id, m_exp.id], &team_filter)
        .await
        .expect("expiry read");
    assert_eq!(live.len(), 1, "expired row excluded");
    assert_eq!(live[0].id, m1.id);

    // touch bumps last_used_at but must not move updated_at (edit audit).
    store.touch_memories(&[m1.id]).await.expect("touch");
    let touched = store
        .get_memories_by_ids(&[m1.id], &team_filter)
        .await
        .expect("read touched");
    assert!(touched[0].last_used_at.is_some(), "last_used_at set");
    assert_eq!(
        touched[0].updated_at, m1.updated_at,
        "touch must not bump updated_at"
    );

    // Update: out-of-scope caller gets NotFound, in-scope succeeds and
    // changes the signature.
    let patch_content = "integration tests must run with --test-threads=1";
    let patch = MemoryPatch {
        content: Some(patch_content.to_string()),
        content_hash: Some(sha256_hex(patch_content)),
        ..Default::default()
    };
    let err = store
        .update_memory(m1.id, &[personal_scope.clone()], &patch, &p1.id)
        .await
        .expect_err("cross-scope update must fail");
    assert!(matches!(err, VedaError::NotFound(_)), "got {err:?}");
    let updated = store
        .update_memory(m1.id, &[team_scope.clone()], &patch, &p1.id)
        .await
        .expect("in-scope update");
    assert_eq!(updated.content, patch_content);

    // Updating another row to identical content collides on the hash.
    let m2 = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Workspace,
            &ws,
            None,
            "some second team fact",
            &p1.id,
        ))
        .await
        .expect("insert m2")
    {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };
    let clash = MemoryPatch {
        content: Some(patch_content.to_string()),
        content_hash: Some(sha256_hex(patch_content)),
        ..Default::default()
    };
    let err = store
        .update_memory(m2.id, &[team_scope.clone()], &clash, &p1.id)
        .await
        .expect_err("hash collision must surface");
    assert!(matches!(err, VedaError::AlreadyExists(_)), "got {err:?}");

    // Delete: wrong scope is a no-op, right scope removes the row, and the
    // read primitive no longer returns it (deleted recall = 0).
    assert!(
        !store
            .delete_memory(m1.id, &[personal_scope.clone()])
            .await
            .expect("cross-scope delete"),
        "cross-scope delete must not match"
    );
    assert!(
        store
            .delete_memory(m1.id, &[team_scope.clone()])
            .await
            .expect("delete"),
        "in-scope delete matches"
    );
    let after = store
        .get_memories_by_ids(&[m1.id], &team_filter)
        .await
        .expect("read after delete");
    assert!(after.is_empty(), "deleted memory must not be readable");

    // Cleanup.
    let pool = store.pool();
    for t in ["veda_memories"] {
        let _ = sqlx::query(&format!(
            "DELETE FROM {t} WHERE scope_id IN (?, ?) OR scope_id = ?"
        ))
        .bind(&ws)
        .bind(&other_ws)
        .bind(&p1.id)
        .execute(pool)
        .await;
    }
    let _ = sqlx::query("DELETE FROM veda_principals WHERE id = ?")
        .bind(&p1.id)
        .execute(pool)
        .await;
}

/// Browse primitives (docs/plans/agent-memory-m4a.md §1.1): enumeration and
/// topic counts carry the same scope guard as every other read, filter
/// expired rows, and page/sort deterministically.
#[tokio::test]
#[ignore]
async fn mysql_memory_browse_list_and_topics() {
    use veda_core::store::MemoryListOrder;

    let cfg = load_config();
    let store = MysqlStore::new(&cfg.mysql.database_url)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let ws = format!("wsm_{}", Uuid::new_v4());
    let other_ws = format!("wsm_{}", Uuid::new_v4());
    let p1 = Uuid::new_v4().to_string();
    let p2 = Uuid::new_v4().to_string();

    let mk = |scope_type: MemoryScopeType,
              scope_id: &str,
              origin: Option<&str>,
              topic: Option<&str>,
              kind: MemoryKind,
              content: &str| NewMemory {
        scope_type,
        scope_id: scope_id.to_string(),
        origin_workspace_id: origin.map(str::to_string),
        topic: topic.map(str::to_string),
        kind,
        content: content.to_string(),
        content_hash: sha256_hex(content),
        source_ref: None,
        expires_at: None,
        created_by: p1.clone(),
    };
    let insert = |m: NewMemory| {
        let store = &store;
        async move {
            match store.insert_memory(&m).await.expect("insert") {
                MemoryInsert::Inserted(m) => m,
                MemoryInsert::Duplicate(_) => panic!("fresh insert reported duplicate"),
            }
        }
    };

    let a = insert(mk(MemoryScopeType::Workspace, &ws, None, Some("testing"), MemoryKind::Fact, "team fact A")).await;
    let b = insert(mk(MemoryScopeType::Workspace, &ws, None, Some("deploy"), MemoryKind::Decision, "team decision B")).await;
    let c = insert(mk(MemoryScopeType::Workspace, &ws, None, None, MemoryKind::Fact, "uncategorized team fact C")).await;
    let mut expired = mk(MemoryScopeType::Workspace, &ws, None, Some("testing"), MemoryKind::Fact, "expired team fact");
    expired.expires_at = Some(Utc::now() - Duration::hours(1));
    let _x = insert(expired).await;
    let n = insert(mk(MemoryScopeType::Principal, &p1, Some(&ws), Some("notes"), MemoryKind::Fact, "project note")).await;
    let p = insert(mk(MemoryScopeType::Principal, &p1, None, None, MemoryKind::Preference, "portable pref")).await;
    let f = insert(mk(MemoryScopeType::Principal, &p1, Some(&other_ws), None, MemoryKind::Fact, "other project note")).await;
    let s = insert(mk(MemoryScopeType::Principal, &p2, None, None, MemoryKind::Fact, "stranger note")).await;

    let team = MemoryScopeFilter::Scope {
        scope_type: MemoryScopeType::Workspace,
        scope_id: ws.clone(),
    };

    // Plain team list: live rows only, expired excluded, total matches.
    let (rows, total) = store
        .list_memories(&team, None, None, MemoryListOrder::UpdatedAt, 1, 50)
        .await
        .expect("team list");
    let mut ids: Vec<i64> = rows.iter().map(|m| m.id).collect();
    ids.sort();
    assert_eq!(ids, {
        let mut v = vec![a.id, b.id, c.id];
        v.sort();
        v
    }, "expired row must not list");
    assert_eq!(total, 3);

    // Topic narrowing: exact, and "" = the uncategorized bucket.
    let (rows, total) = store
        .list_memories(&team, Some("testing"), None, MemoryListOrder::UpdatedAt, 1, 50)
        .await
        .expect("topic list");
    assert_eq!((rows.len(), total), (1, 1));
    assert_eq!(rows[0].id, a.id, "live testing row only");
    let (rows, _) = store
        .list_memories(&team, Some(""), None, MemoryListOrder::UpdatedAt, 1, 50)
        .await
        .expect("uncategorized list");
    assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), vec![c.id]);

    // Kind narrowing.
    let (rows, _) = store
        .list_memories(&team, None, Some(MemoryKind::Decision), MemoryListOrder::UpdatedAt, 1, 50)
        .await
        .expect("kind list");
    assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), vec![b.id]);

    // Pagination: size 1 walks all three, total stays 3 on every page.
    let mut seen = vec![];
    for page in 1..=3u32 {
        let (rows, total) = store
            .list_memories(&team, None, None, MemoryListOrder::UpdatedAt, page, 1)
            .await
            .expect("page");
        assert_eq!((rows.len(), total), (1, 3), "page {page}");
        seen.push(rows[0].id);
    }
    seen.sort();
    assert_eq!(seen, ids, "pages cover every row exactly once");

    // Heat order: the one touched row leads, never-retrieved rows sink.
    store.touch_memories(&[b.id]).await.expect("touch");
    let (rows, _) = store
        .list_memories(&team, None, None, MemoryListOrder::LastUsedAt, 1, 50)
        .await
        .expect("heat list");
    assert_eq!(rows[0].id, b.id, "touched row leads the heat view");

    // Personal filter: origin ∈ {ws, none}; foreign-origin and other
    // principals' rows are invisible in BOTH directions (leak = 0).
    let mine = MemoryScopeFilter::Personal {
        principal_id: p1.clone(),
        workspace_id: ws.clone(),
    };
    let (rows, total) = store
        .list_memories(&mine, None, None, MemoryListOrder::UpdatedAt, 1, 50)
        .await
        .expect("mine list");
    let mut got: Vec<i64> = rows.iter().map(|m| m.id).collect();
    got.sort();
    assert_eq!(got, {
        let mut v = vec![n.id, p.id];
        v.sort();
        v
    }, "mine = portable + pinned-here, no foreign origin, no strangers");
    assert_eq!(total, 2);
    let (rows, _) = store
        .list_memories(
            &MemoryScopeFilter::Personal {
                principal_id: p2.clone(),
                workspace_id: ws.clone(),
            },
            None,
            None,
            MemoryListOrder::UpdatedAt,
            1,
            50,
        )
        .await
        .expect("stranger mine list");
    assert_eq!(rows.iter().map(|m| m.id).collect::<Vec<_>>(), vec![s.id]);
    assert!(!rows.iter().any(|m| m.id == f.id || m.id == n.id));

    // Topic directory: live rows only, uncategorized bucket included.
    let counts = store.topic_counts(&team).await.expect("topic counts");
    assert_eq!(counts.len(), 3, "testing/deploy/uncategorized: {counts:?}");
    for want in [
        (Some("testing".to_string()), 1i64),
        (Some("deploy".to_string()), 1),
        (None, 1),
    ] {
        assert!(counts.contains(&want), "missing {want:?} in {counts:?}");
    }

    // Cleanup.
    let pool = store.pool();
    let _ = sqlx::query("DELETE FROM veda_memories WHERE scope_id IN (?, ?, ?)")
        .bind(&ws)
        .bind(&p1)
        .bind(&p2)
        .execute(pool)
        .await;
}

/// Codex review round (2026-08-12): memory writes must commit their
/// MemorySync heal task in the SAME transaction (the fs write-path
/// invariant), and an outbox row with an event_type this binary doesn't
/// know must dead-letter alone instead of poisoning the whole claim.
#[tokio::test]
#[ignore]
async fn mysql_memory_outbox_tasks_and_claim_resilience() {
    let cfg = load_config();
    let store = MysqlStore::new(&cfg.mysql.database_url)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = store.pool();

    let ws = format!("wsm_{}", Uuid::new_v4());
    let team_scope = (MemoryScopeType::Workspace, ws.clone());

    let count_tasks = |op: &'static str, id: i64| {
        let pool = pool.clone();
        async move {
            let row: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM veda_outbox WHERE event_type = 'memory_sync' \
                 AND payload->>'$.op' = ? AND payload->>'$.memory_id' = ?",
            )
            .bind(op)
            .bind(id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            row.0
        }
    };

    // insert → one upsert task committed with the row
    let m = match store
        .insert_memory(&new_memory(
            MemoryScopeType::Workspace,
            &ws,
            None,
            "durable fact",
            "p-test",
        ))
        .await
        .unwrap()
    {
        MemoryInsert::Inserted(m) => m,
        _ => panic!("dup"),
    };
    assert_eq!(count_tasks("upsert", m.id).await, 1, "insert commits a heal task");

    // content update → second upsert task; metadata-only update → no new task
    let patch = MemoryPatch {
        content: Some("durable fact, corrected".into()),
        content_hash: Some(sha256_hex("durable fact, corrected")),
        ..Default::default()
    };
    store
        .update_memory(m.id, &[team_scope.clone()], &patch, "p-test")
        .await
        .unwrap();
    assert_eq!(count_tasks("upsert", m.id).await, 2, "content update commits a task");
    let meta_patch = MemoryPatch {
        topic: Some("durability".into()),
        ..Default::default()
    };
    store
        .update_memory(m.id, &[team_scope.clone()], &meta_patch, "p-test")
        .await
        .unwrap();
    assert_eq!(
        count_tasks("upsert", m.id).await,
        2,
        "metadata-only update must not re-embed"
    );

    // delete → delete task
    assert!(store.delete_memory(m.id, &[team_scope.clone()]).await.unwrap());
    assert_eq!(count_tasks("delete", m.id).await, 1, "delete commits a delete task");

    // claim resilience: a row with an unknown event_type dead-letters alone
    sqlx::query(
        "INSERT INTO veda_outbox (workspace_id, event_type, payload, status, retry_count, \
         max_retries, available_at, created_at) \
         VALUES (?, 'from_the_future', '{}', 'pending', 0, 3, UTC_TIMESTAMP(), UTC_TIMESTAMP())",
    )
    .bind(&ws)
    .execute(pool)
    .await
    .unwrap();
    let claimed = store.claim(500).await.expect("claim must survive unknown event_type");
    assert!(
        claimed.iter().all(|e| e.workspace_id != ws || e.event_type == veda_types::OutboxEventType::MemorySync),
        "only parsable rows come back"
    );
    let (status,): (String,) = sqlx::query_as(
        "SELECT status FROM veda_outbox WHERE workspace_id = ? AND event_type = 'from_the_future'",
    )
    .bind(&ws)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(status, "dead", "unknown event_type row dead-letters alone");

    // cleanup: our outbox rows (claimed ones included) + memory rows
    let _ = sqlx::query("DELETE FROM veda_outbox WHERE workspace_id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
    // release unrelated rows this claim grabbed so other suites aren't
    // blocked by a 10-minute lease
    for e in claimed.iter().filter(|e| e.workspace_id != ws) {
        let _ = sqlx::query(
            "UPDATE veda_outbox SET status = 'pending', lease_until = NULL WHERE id = ? AND status = 'processing'",
        )
        .bind(e.id)
        .execute(pool)
        .await;
    }
    let _ = sqlx::query("DELETE FROM veda_memories WHERE scope_id = ?")
        .bind(&ws)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore]
async fn milvus_memory_index_scope_filter_and_delete() {
    let cfg = load_config();
    let dim = cfg.embedding.and_then(|e| e.dimension).unwrap_or(1024) as usize;
    let store = MilvusStore::new(&cfg.milvus.url, cfg.milvus.token, cfg.milvus.db);
    store
        .init_memory_collection(dim as u32)
        .await
        .expect("init memory collection");

    // Unique scopes per run keep the shared collection from polluting
    // filters across runs; ids are micros-based to avoid pk collisions.
    let ws = format!("wsm_{}", Uuid::new_v4());
    let other_ws = format!("wsm_{}", Uuid::new_v4());
    let principal = Uuid::new_v4().to_string();
    let base = Utc::now().timestamp_micros();

    let mk = |off: i64, scope_type: MemoryScopeType, scope_id: &str, origin: &str| {
        let mut v = vec![0.01_f32; dim];
        v[(off % dim as i64) as usize] = 1.0;
        MemoryWithEmbedding {
            id: base + off,
            scope_type,
            scope_id: scope_id.to_string(),
            origin_workspace_id: origin.to_string(),
            content: format!("memory row {off}"),
            vector: v,
        }
    };
    let team = mk(0, MemoryScopeType::Workspace, &ws, "");
    let note = mk(1, MemoryScopeType::Principal, &principal, &ws);
    let pref = mk(2, MemoryScopeType::Principal, &principal, "");
    let foreign = mk(3, MemoryScopeType::Principal, &principal, &other_ws);
    store
        .upsert_memory_vectors(&[team.clone(), note.clone(), pref.clone(), foreign.clone()])
        .await
        .expect("upsert vectors");

    let probe = vec![0.01_f32; dim];

    // Single-scope filter sees only the team row.
    let team_only = store
        .search_memory_candidates(
            "memory row",
            &probe,
            &MemoryScopeFilter::Scope {
                scope_type: MemoryScopeType::Workspace,
                scope_id: ws.clone(),
            },
            10,
        )
        .await
        .expect("scope search");
    let ids: Vec<i64> = team_only.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![team.id], "workspace scope isolates: {ids:?}");

    // Context union: team + personal with origin ∈ {"", ws}; the foreign-
    // origin note stays invisible.
    let ctx = store
        .search_memory_candidates(
            "memory row",
            &probe,
            &MemoryScopeFilter::Context {
                workspace_id: ws.clone(),
                principal_id: principal.clone(),
                dept_id: None,
            },
            10,
        )
        .await
        .expect("context search");
    let mut ids: Vec<i64> = ctx.iter().map(|c| c.id).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![team.id, note.id, pref.id],
        "context = team + origin-matched personal"
    );

    // Delete propagates: candidates stop coming back (Strong consistency).
    store
        .delete_memory_vectors(&[team.id, note.id, pref.id, foreign.id])
        .await
        .expect("delete vectors");
    let after = store
        .search_memory_candidates(
            "memory row",
            &probe,
            &MemoryScopeFilter::Context {
                workspace_id: ws.clone(),
                principal_id: principal.clone(),
                dept_id: None,
            },
            10,
        )
        .await
        .expect("search after delete");
    assert!(after.is_empty(), "deleted vectors still searchable: {after:?}");
}

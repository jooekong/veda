//! Milvus integration tests. Run with: `cargo test -p veda-store -- --ignored`
//! If delete visibility lags, run with `--test-threads=1`.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use uuid::Uuid;
use serde_json::json;
use veda_core::store::{CollectionVectorStore, VectorStore, VectorWorkspaceStore};
use veda_store::MilvusStore;
use veda_types::{
    ChunkWithEmbedding, FieldDefinition, SearchMode, SearchRequest, UpsertRecord,
};

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
    milvus: MilvusSection,
    embedding: Option<EmbeddingSection>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn load_milvus() -> (String, Option<String>, Option<String>) {
    let path = workspace_root().join("config/test.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg: TestToml = toml::from_str(&raw).expect("parse test.toml [milvus]");
    (cfg.milvus.url, cfg.milvus.token, cfg.milvus.db)
}

fn load_embedding_dim() -> u32 {
    let path = workspace_root().join("config/test.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg: TestToml = toml::from_str(&raw).expect("parse test.toml");
    cfg.embedding.and_then(|e| e.dimension).unwrap_or(1024)
}

#[tokio::test]
#[ignore]
async fn milvus_init_upsert_search_delete() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    store.init_collections(dim).await.expect("init");

    let ws = format!("ws_{}", Uuid::new_v4());
    let fid = Uuid::new_v4().to_string();
    let vec: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.01).collect();
    let chunks = vec![ChunkWithEmbedding {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.clone(),
        file_id: fid.clone(),
        chunk_index: 0,
        content: "hello milvus integration".into(),
        vector: vec.clone(),
    }];
    store.upsert_chunks(&chunks).await.expect("upsert");

    let mut req = SearchRequest {
        workspace_id: ws.clone(),
        query: "".into(),
        mode: SearchMode::Semantic,
        limit: 5,
        path_prefix: None,
        query_vector: Some(vec.clone()),
    };
    let mut found = false;
    for _ in 0..10 {
        let hits = store.search(&req).await.expect("search");
        if hits.iter().any(|h| h.file_id == fid) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(found, "upserted chunk should be searchable");

    let hy = SearchRequest {
        workspace_id: ws.clone(),
        query: "hello".into(),
        mode: SearchMode::Hybrid,
        limit: 5,
        path_prefix: None,
        query_vector: Some(vec.clone()),
    };
    let _ = store.search(&hy).await.expect("hybrid");

    store.delete_chunks(&ws, &fid).await.expect("delete_chunks");

    req.query_vector = Some(vec);
    let mut gone = false;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let hits2 = store.search(&req).await.expect("search after delete");
        if !hits2.iter().any(|h| h.file_id == fid) {
            gone = true;
            break;
        }
    }
    assert!(
        gone,
        "vector rows for file_id should disappear after delete"
    );
}

#[tokio::test]
#[ignore]
async fn milvus_dynamic_collection_crud() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);

    let coll_name = format!("veda_test_{}", Uuid::new_v4().to_string().replace('-', "_"));
    let fields = vec![
        FieldDefinition {
            name: "title".into(),
            field_type: "string".into(),
            index: true,
        },
        FieldDefinition {
            name: "content".into(),
            field_type: "string".into(),
            index: false,
        },
    ];

    let dim = load_embedding_dim();
    store
        .create_dynamic_collection(&coll_name, &fields, dim)
        .await
        .expect("create dynamic collection");

    let ws = format!("ws_{}", Uuid::new_v4());
    let vec1: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.1).collect();
    let rows = vec![serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "title": "Test Article",
        "content": "This is a test article about Rust programming.",
        "vector": vec1,
    })];
    store
        .insert_collection_rows(&coll_name, &ws, &rows)
        .await
        .expect("insert rows");

    let results = store
        .search_collection(&coll_name, &ws, &vec1, 5)
        .await
        .expect("search collection");
    assert!(!results.is_empty());
    let first = &results[0];
    assert_eq!(
        first.get("title").and_then(|v| v.as_str()),
        Some("Test Article")
    );

    store
        .drop_dynamic_collection(&coll_name)
        .await
        .expect("drop collection");
}

#[tokio::test]
#[ignore]
async fn milvus_vector_collection_create_and_drop() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();

    let ws_id = Uuid::new_v4().to_string();
    let expected_name = veda_store::vector_collection_name(&ws_id);

    // create_vector_collection should: 1) accept the full §2.2 schema (incl
    // Array<VarChar> with max_capacity, nullable Int64, BM25 function), 2)
    // build all 7 indexes, 3) load the collection. This validates DDL +
    // index payload shapes against real Milvus 2.6.14 — write-side semantics
    // (max_capacity enforcement, dim mismatch, BM25 input vs output handling)
    // are validated in Stage 4 insert tests.
    let name = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("create_vector_collection");
    assert_eq!(name, expected_name);

    // drop_collection should remove it cleanly.
    store
        .drop_collection(&name)
        .await
        .expect("drop_collection");
}

#[tokio::test]
#[ignore]
async fn milvus_vector_collection_create_is_idempotent() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();

    let ws_id = Uuid::new_v4().to_string();

    let name1 = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("first create");
    // Second create on the same workspace_id must not error — Stage 2.1
    // added the "CollectionAlreadyExists" swallow.
    let name2 = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("second create (idempotent)");
    assert_eq!(name1, name2);

    store.drop_collection(&name1).await.expect("drop");
}

#[tokio::test]
#[ignore]
async fn milvus_drop_collection_is_idempotent() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();

    let ws_id = Uuid::new_v4().to_string();
    let name = veda_store::vector_collection_name(&ws_id);

    // Drop a never-created collection — must succeed (not-exists swallow).
    store
        .drop_collection(&name)
        .await
        .expect("drop non-existent");

    // Create, drop, drop again — second drop must succeed.
    store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("create");
    store.drop_collection(&name).await.expect("first drop");
    store.drop_collection(&name).await.expect("second drop");
}

#[tokio::test]
#[ignore]
async fn milvus_vector_data_plane_roundtrip() {
    // Exercises the full Stage 4.2/4.3 wire shapes against real Milvus 2.6.14:
    // upsert (incl. tags Array, nullable expire_at, BM25 sparse auto-gen) →
    // search by ANN → query by pk array → delete by `pk in [...]`.
    // Catches REST shape bugs cheaply; without it, Stage 4.4 would build
    // filter DSL on top of an unverified data plane.

    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();

    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("create");

    let pk1 = format!("default:rk1-{}", &Uuid::new_v4().to_string()[..8]);
    let pk2 = format!("default:rk2-{}", &Uuid::new_v4().to_string()[..8]);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mk_vector = |seed: f32| -> Vec<f32> {
        (0..dim).map(|i| seed + (i as f32) * 0.001).collect()
    };
    let records = vec![
        UpsertRecord {
            pk: pk1.clone(),
            row_key: pk1.strip_prefix("default:").unwrap().to_string(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec!["sale".into(), "new".into()],
            status: "active".into(),
            text: "hello milvus data plane".into(),
            vector: mk_vector(0.1),
            meta: json!({ "price": 42 }),
            expire_at: None,
            created_at: now_ms,
            updated_at: now_ms,
        },
        UpsertRecord {
            pk: pk2.clone(),
            row_key: pk2.strip_prefix("default:").unwrap().to_string(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec![],
            status: "active".into(),
            text: "another vector record".into(),
            vector: mk_vector(0.5),
            meta: json!({}),
            expire_at: Some(now_ms + 86_400_000),
            created_at: now_ms,
            updated_at: now_ms,
        },
    ];

    // 1. Upsert via the trait (commit_ts is server-now).
    let commit_ts = VectorWorkspaceStore::upsert_records(&store, &ws_id, &records)
        .await
        .expect("upsert");
    assert!(commit_ts >= now_ms, "commit_ts must be >= now_ms");

    // Milvus is eventually consistent — give the index a moment to load
    // the freshly upserted rows. ~1s in practice on the test cluster.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 2. Search using the first record's vector — top hit should be pk1.
    let hits = VectorWorkspaceStore::search_vectors(
        &store, &ws_id, "default", &mk_vector(0.1), 2,
    )
    .await
    .expect("search");
    assert!(!hits.is_empty(), "search returned no hits");
    let top = &hits[0];
    assert_eq!(top.pk, pk1, "expected top hit pk={pk1}, got {}", top.pk);
    assert_eq!(top.dataset, "default");
    assert_eq!(top.category, "default");
    assert_eq!(top.tags, vec!["sale".to_string(), "new".to_string()]);
    assert_eq!(top.status, "active");
    assert_eq!(top.text, "hello milvus data plane");
    assert_eq!(top.meta["price"], 42);

    // 3. Query by pk array.
    let pks = vec![pk1.clone(), pk2.clone()];
    let results = VectorWorkspaceStore::query_vectors_by_pk(&store, &ws_id, &pks)
        .await
        .expect("query");
    assert_eq!(results.len(), 2, "expected 2 hits, got {}", results.len());
    // Order not preserved; index by pk.
    let by_pk: std::collections::HashMap<_, _> =
        results.into_iter().map(|h| (h.pk.clone(), h)).collect();
    assert!(by_pk.contains_key(&pk1));
    assert!(by_pk.contains_key(&pk2));
    // Verify nullable expire_at round-trip: pk1 None, pk2 Some.
    assert_eq!(by_pk[&pk1].expire_at, None);
    assert!(by_pk[&pk2].expire_at.is_some());

    // 4. Delete both pks.
    let accepted = VectorWorkspaceStore::delete_vectors_by_pk(&store, &ws_id, &pks)
        .await
        .expect("delete");
    assert_eq!(accepted, 2);

    // Allow delete to propagate.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 5. Verify gone.
    let after = VectorWorkspaceStore::query_vectors_by_pk(&store, &ws_id, &pks)
        .await
        .expect("query after delete");
    assert!(after.is_empty(), "expected 0 hits after delete, got {}", after.len());

    // Cleanup.
    store
        .drop_collection(&collection_name)
        .await
        .expect("drop");
}

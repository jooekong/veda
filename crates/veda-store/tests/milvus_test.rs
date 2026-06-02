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
    VectorSearchQuery,
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
            id: pk1.strip_prefix("default:").unwrap().to_string(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec!["sale".into(), "new".into()],
            text: "hello milvus data plane".into(),
            vector: mk_vector(0.1),
            meta: json!({ "price": 42 }),
            created_at: now_ms,
            updated_at: now_ms,
        },
        UpsertRecord {
            pk: pk2.clone(),
            id: pk2.strip_prefix("default:").unwrap().to_string(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec![],
            text: "another vector record".into(),
            vector: mk_vector(0.5),
            meta: json!({}),
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
        &store,
        &ws_id,
        "default",
        VectorSearchQuery::Semantic { vector: &mk_vector(0.1) },
        2,
        None,
        None,
    )
    .await
    .expect("search");
    assert!(!hits.is_empty(), "search returned no hits");
    let top = &hits[0];
    let expected_id1 = pk1.strip_prefix("default:").unwrap();
    assert_eq!(top.id, expected_id1, "expected top hit id={expected_id1}, got {}", top.id);
    assert_eq!(top.dataset.as_deref(), Some("default"));
    assert_eq!(top.category.as_deref(), Some("default"));
    assert_eq!(top.tags, Some(vec!["sale".to_string(), "new".to_string()]));
    assert_eq!(top.text.as_deref(), Some("hello milvus data plane"));
    assert_eq!(top.meta.as_ref().unwrap()["price"], 42);

    // 3. Query by pk array.
    let pks = vec![pk1.clone(), pk2.clone()];
    let results = VectorWorkspaceStore::query_vectors_by_pk(&store, &ws_id, &pks, None)
        .await
        .expect("query");
    assert_eq!(results.len(), 2, "expected 2 hits, got {}", results.len());
    // Order not preserved; index by id.
    let by_id: std::collections::HashMap<_, _> =
        results.into_iter().map(|h| (h.id.clone(), h)).collect();
    let id1 = pk1.strip_prefix("default:").unwrap().to_string();
    let id2 = pk2.strip_prefix("default:").unwrap().to_string();
    assert!(by_id.contains_key(&id1));
    assert!(by_id.contains_key(&id2));

    // 4. Delete both pks.
    let accepted = VectorWorkspaceStore::delete_vectors_by_pk(&store, &ws_id, &pks)
        .await
        .expect("delete");
    assert_eq!(accepted, 2);

    // Allow delete to propagate.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 5. Verify gone.
    let after = VectorWorkspaceStore::query_vectors_by_pk(&store, &ws_id, &pks, None)
        .await
        .expect("query after delete");
    assert!(after.is_empty(), "expected 0 hits after delete, got {}", after.len());

    // Cleanup.
    store
        .drop_collection(&collection_name)
        .await
        .expect("drop");
}

/// Helper: upsert N synthetic records spread across the given dataset
/// names. Used by filter / isolation / batch tests.
async fn seed_records(
    store: &MilvusStore,
    ws_id: &str,
    dim: u32,
    by_dataset: &[(&str, Vec<(&str, serde_json::Value)>)],
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mk_vector = |seed: f32| -> Vec<f32> {
        (0..dim).map(|i| seed + (i as f32) * 0.001).collect()
    };
    let mut records = Vec::new();
    for (dataset, rows) in by_dataset {
        for (i, (rk, meta)) in rows.iter().enumerate() {
            records.push(UpsertRecord {
                pk: format!("{dataset}:{rk}"),
                id: rk.to_string(),
                dataset: dataset.to_string(),
                category: "default".into(),
                tags: vec![],
                text: format!("seed-{dataset}-{rk}"),
                vector: mk_vector(0.1 + (i as f32) * 0.1),
                meta: meta.clone(),
                created_at: now_ms,
                updated_at: now_ms,
            });
        }
    }
    VectorWorkspaceStore::upsert_records(store, ws_id, &records)
        .await
        .expect("seed upsert");
    // Allow Milvus indexes a moment to catch up.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
}

#[tokio::test]
#[ignore]
async fn milvus_search_with_filter_dsl_eq_and_range() {
    // Validates that the Filter DSL → Milvus expr the parser generates
    // (Stage 4.4) is actually accepted by Milvus 2.6.14 and filters as
    // expected. Without this real-service check, unit tests can only
    // prove string SHAPE, not that Milvus interprets `meta["x"]` syntax
    // the way we expect.
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store.create_vector_collection(&ws_id, dim).await.unwrap();

    seed_records(
        &store,
        &ws_id,
        dim,
        &[(
            "default",
            vec![
                ("a", json!({"price": 50, "category": "shoes"})),
                ("b", json!({"price": 150, "category": "shoes"})),
                ("c", json!({"price": 80, "category": "electronics"})),
            ],
        )],
    )
    .await;

    let query = (0..dim).map(|i| (i as f32) * 0.001).collect::<Vec<_>>();

    // Range filter: price < 100 → expect a (50) + c (80), exclude b (150).
    let hits = VectorWorkspaceStore::search_vectors(
        &store,
        &ws_id,
        "default",
        VectorSearchQuery::Semantic { vector: &query },
        10,
        Some(r#"meta["price"] < 100"#),
        None,
    )
    .await
    .expect("search with range filter");
    let ids: std::collections::HashSet<_> =
        hits.iter().map(|h| h.id.clone()).collect();
    assert!(ids.contains("a"), "expected a in {ids:?}");
    assert!(ids.contains("c"), "expected c in {ids:?}");
    assert!(!ids.contains("b"), "b should be excluded; got {ids:?}");

    // Eq filter on string: category == "shoes" → a + b, not c.
    let hits = VectorWorkspaceStore::search_vectors(
        &store,
        &ws_id,
        "default",
        VectorSearchQuery::Semantic { vector: &query },
        10,
        Some(r#"meta["category"] == "shoes""#),
        None,
    )
    .await
    .expect("search with eq filter");
    let ids: std::collections::HashSet<_> =
        hits.iter().map(|h| h.id.clone()).collect();
    assert!(ids.contains("a"));
    assert!(ids.contains("b"));
    assert!(!ids.contains("c"));

    store.drop_collection(&collection_name).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn milvus_search_with_in_or_expansion() {
    // Filter DSL `in` operator is implemented as an OR-chain expansion
    // (Codex Stage 4.4 design review — avoiding Milvus 2.6 JSON-path
    // TermExpr ambiguity). Verify the generated expression filters
    // as expected.
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store.create_vector_collection(&ws_id, dim).await.unwrap();

    seed_records(
        &store,
        &ws_id,
        dim,
        &[(
            "default",
            vec![
                ("a", json!({"brand": "nike"})),
                ("b", json!({"brand": "adidas"})),
                ("c", json!({"brand": "puma"})),
            ],
        )],
    )
    .await;

    let query = (0..dim).map(|i| (i as f32) * 0.001).collect::<Vec<_>>();
    let hits = VectorWorkspaceStore::search_vectors(
        &store,
        &ws_id,
        "default",
        VectorSearchQuery::Semantic { vector: &query },
        10,
        Some(r#"(meta["brand"] == "nike" || meta["brand"] == "adidas")"#),
        None,
    )
    .await
    .expect("search with OR-chain");
    let ids: std::collections::HashSet<_> =
        hits.iter().map(|h| h.id.clone()).collect();
    assert!(ids.contains("a"));
    assert!(ids.contains("b"));
    assert!(!ids.contains("c"), "c should be excluded");

    store.drop_collection(&collection_name).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn milvus_multi_dataset_isolation() {
    // Validates that `dataset == "X"` in the base filter actually isolates
    // datasets within the same workspace collection. Without this, a
    // search against dataset "ds_a" could leak rows from "ds_b".
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store.create_vector_collection(&ws_id, dim).await.unwrap();

    seed_records(
        &store,
        &ws_id,
        dim,
        &[
            ("ds_a", vec![("rk1", json!({})), ("rk2", json!({}))]),
            ("ds_b", vec![("rk3", json!({})), ("rk4", json!({}))]),
        ],
    )
    .await;

    let query = (0..dim).map(|i| (i as f32) * 0.001).collect::<Vec<_>>();
    let hits = VectorWorkspaceStore::search_vectors(
        &store,
        &ws_id,
        "ds_a",
        VectorSearchQuery::Semantic { vector: &query },
        10,
        None,
        None,
    )
    .await
    .expect("search ds_a");
    assert!(hits.iter().all(|h| h.dataset.as_deref() == Some("ds_a")),
            "ds_a search returned cross-dataset hits: {:?}",
            hits.iter().map(|h| &h.dataset).collect::<Vec<_>>());
    let ds_a_keys: std::collections::HashSet<_> =
        hits.iter().map(|h| h.id.clone()).collect();
    assert!(ds_a_keys.contains("rk1"));
    assert!(ds_a_keys.contains("rk2"));
    assert!(!ds_a_keys.contains("rk3"));
    assert!(!ds_a_keys.contains("rk4"));

    store.drop_collection(&collection_name).await.unwrap();
}

/// Proves db Fulltext mode actually queries the BM25 inverted index rather than
/// some degraded path. Two records share the SAME dense vector (so ANN cannot
/// tell them apart); only one contains a distinctive token. A fulltext query
/// for that token must return that record and NOT the other (BM25 sparse search
/// only returns docs sharing query terms). A correct result is therefore
/// attributable to BM25 alone — closing the F1 blind spot for the db path.
#[tokio::test]
#[ignore]
async fn db_fulltext_finds_lexical_only_hit() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("create");

    let noise: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let records = vec![
        UpsertRecord {
            pk: "default:has-token".into(),
            id: "has-token".into(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec![],
            text: "the quarterly invoice zqxwprodcode was approved by finance".into(),
            vector: noise.clone(),
            meta: json!({}),
            created_at: now_ms,
            updated_at: now_ms,
        },
        UpsertRecord {
            pk: "default:no-token".into(),
            id: "no-token".into(),
            dataset: "default".into(),
            category: "default".into(),
            tags: vec![],
            text: "a totally different sentence about weather and rivers".into(),
            vector: noise.clone(),
            meta: json!({}),
            created_at: now_ms,
            updated_at: now_ms,
        },
    ];
    VectorWorkspaceStore::upsert_records(&store, &ws_id, &records)
        .await
        .expect("upsert");

    // Poll: distinguish "BM25 index still loading" from "BM25 path broken".
    let mut hits = Vec::new();
    for _ in 0..15 {
        hits = VectorWorkspaceStore::search_vectors(
            &store,
            &ws_id,
            "default",
            VectorSearchQuery::Fulltext { text: "zqxwprodcode" },
            10,
            None,
            None,
        )
        .await
        .expect("fulltext search");
        if !hits.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    assert!(
        !hits.is_empty(),
        "fulltext returned no hits — BM25 path not working"
    );
    assert_eq!(
        hits[0].id, "has-token",
        "top hit should be the doc containing the token"
    );
    assert_eq!(hits[0].score_type, "bm25", "score_type must mark this as bm25");
    assert!(
        !hits.iter().any(|h| h.id == "no-token"),
        "doc without the token must not match a BM25 query (proves inverted-index semantics, not substring/degraded)"
    );

    store.drop_collection(&collection_name).await.unwrap();
}

/// Falsification test for the fs (veda_chunks) BM25 path — review findings
/// F1/F2. The existing `query_fulltext` puts `metricType` at the body top level
/// (not the Milvus 2.6 official REST shape), and no test ever asserted it
/// returns real lexical hits (hybrid silently falls back to ANN, so a broken
/// BM25 path stays green). This asserts a token-bearing chunk is found by
/// fulltext and a token-free one is not. If fs's REST shape is wrong, this
/// fails — surfacing the latent bug instead of hiding it.
#[tokio::test]
#[ignore]
async fn fs_fulltext_finds_lexical_only_hit() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    store.init_collections(dim).await.expect("init");

    let ws = format!("ws_{}", Uuid::new_v4());
    let noise: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();
    let fid_hit = Uuid::new_v4().to_string();
    let fid_miss = Uuid::new_v4().to_string();
    store
        .upsert_chunks_only(&[
            ChunkWithEmbedding {
                id: Uuid::new_v4().to_string(),
                workspace_id: ws.clone(),
                file_id: fid_hit.clone(),
                chunk_index: 0,
                content: "release notes mention zqxwprodcode shipping next week".into(),
                vector: noise.clone(),
            },
            ChunkWithEmbedding {
                id: Uuid::new_v4().to_string(),
                workspace_id: ws.clone(),
                file_id: fid_miss.clone(),
                chunk_index: 0,
                content: "unrelated paragraph about gardening and soil".into(),
                vector: noise.clone(),
            },
        ])
        .await
        .expect("upsert chunks");

    let req = SearchRequest {
        workspace_id: ws.clone(),
        query: "zqxwprodcode".into(),
        mode: SearchMode::Fulltext,
        limit: 10,
        path_prefix: None,
        query_vector: None,
    };
    let mut hits = Vec::new();
    for _ in 0..15 {
        hits = store.search(&req).await.expect("fs fulltext search");
        if !hits.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    assert!(
        !hits.is_empty(),
        "fs fulltext returned no hits — F1/F2 confirmed (BM25 path broken)"
    );
    assert_eq!(
        hits[0].file_id, fid_hit,
        "top hit should be the chunk containing the token"
    );
    assert_eq!(hits[0].score_type, "bm25");
    assert!(
        !hits.iter().any(|h| h.file_id == fid_miss),
        "token-free chunk must not match BM25 query"
    );

    store.delete_chunks(&ws, &fid_hit).await.ok();
    store.delete_chunks(&ws, &fid_miss).await.ok();
}

/// Proves db Hybrid genuinely FUSES dense + BM25 (not just runs one ranker) and
/// that the `entities/hybrid_search` response parses the complex columns
/// (`tags` Array, `meta` JSON) the same way `entities/search` does.
///
/// Setup: a token-bearing record `T` is placed FAR in dense space (behind 3
/// distractors), so pure-dense top-3 would exclude it. A fulltext query token
/// + a query vector near the non-token docs are issued together. If hybrid
/// fuses, BM25 pulls `T` into the top-3; pure dense never could. Also asserts
/// `T`'s tags/meta round-trip and every hit is `score_type == "rrf"`.
#[tokio::test]
#[ignore]
async fn db_hybrid_fuses_and_returns_complex_fields() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string();
    let collection_name = store
        .create_vector_collection(&ws_id, dim)
        .await
        .expect("create");

    let mk = |s: f32| -> Vec<f32> { (0..dim).map(|i| s + (i as f32) * 0.001).collect() };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rec = |id: &str, seed: f32, text: &str, tags: Vec<String>, meta: serde_json::Value| {
        UpsertRecord {
            pk: format!("default:{id}"),
            id: id.to_string(),
            dataset: "default".into(),
            category: "default".into(),
            tags,
            text: text.into(),
            vector: mk(seed),
            meta,
            created_at: now_ms,
            updated_at: now_ms,
        }
    };
    // V + 3 distractors are all closer to the query vector (seed 0.10) than the
    // token doc T (seed 0.90); pure-dense top-3 = {V, d1, d2}, T is rank 5.
    let records = vec![
        rec("vmatch", 0.10, "alpha beta gamma no special token", vec![], json!({})),
        rec("d1", 0.11, "filler one", vec![], json!({})),
        rec("d2", 0.12, "filler two", vec![], json!({})),
        rec("d3", 0.13, "filler three", vec![], json!({})),
        rec(
            "tmatch",
            0.90,
            "this record mentions zqxwprodcode explicitly",
            vec!["ttag".into(), "sale".into()],
            json!({ "kind": "t", "n": 7 }),
        ),
    ];
    VectorWorkspaceStore::upsert_records(&store, &ws_id, &records)
        .await
        .expect("upsert");

    let mut hits = Vec::new();
    for _ in 0..15 {
        hits = VectorWorkspaceStore::search_vectors(
            &store,
            &ws_id,
            "default",
            VectorSearchQuery::Hybrid {
                vector: &mk(0.10),
                text: "zqxwprodcode",
            },
            3,
            None,
            None,
        )
        .await
        .expect("hybrid search");
        // Wait until the token doc is fused in (index may lag right after upsert).
        if hits.iter().any(|h| h.id == "tmatch") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    assert!(!hits.is_empty(), "hybrid returned no hits");
    assert!(
        hits.iter().all(|h| h.score_type == "rrf"),
        "every hybrid hit must be score_type=rrf, got {:?}",
        hits.iter().map(|h| &h.score_type).collect::<Vec<_>>()
    );
    // Two-way fusion proof: `tmatch` can only come from BM25 (dense-far), and
    // `vmatch` can only come from dense (no token, so BM25 ignores it). Both in
    // top-3 ⇒ both rankers contributed — not a sparse-only or dense-only path.
    assert!(
        hits.iter().any(|h| h.id == "vmatch"),
        "dense-near doc must be present (proves dense ranker contributes to the fusion)"
    );
    let t = hits
        .iter()
        .find(|h| h.id == "tmatch")
        .expect("BM25 must fuse the dense-far token doc into top-3 (proves BM25 ranker contributes)");
    // Complex columns must parse from the hybrid_search response shape.
    assert_eq!(
        t.tags,
        Some(vec!["ttag".to_string(), "sale".to_string()]),
        "tags Array must parse from hybrid response"
    );
    let meta = t.meta.as_ref().expect("meta present");
    assert_eq!(meta["kind"], "t", "meta JSON must parse from hybrid response");
    assert_eq!(meta["n"], 7);

    store.drop_collection(&collection_name).await.unwrap();
}

/// Hybrid must surface backend errors rather than swallow them into an empty
/// `Ok` (and per decision D4 there is NO silent fallback to semantic — that
/// is structural: the Hybrid arm has no fallback branch). Querying a workspace
/// whose collection was never provisioned makes Milvus error; we assert that
/// propagates as `Err`.
#[tokio::test]
#[ignore]
async fn db_hybrid_surfaces_error() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    let ws_id = Uuid::new_v4().to_string(); // never provisioned → no collection
    let vector: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.001).collect();

    let result = VectorWorkspaceStore::search_vectors(
        &store,
        &ws_id,
        "default",
        VectorSearchQuery::Hybrid {
            vector: &vector,
            text: "anything",
        },
        5,
        None,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "hybrid against a missing collection must Err, not return empty Ok (got {:?})",
        result.map(|h| h.len())
    );
}

/// Falsification test for the fs (veda_chunks) hybrid path — review finding F1.
/// fs defaults to Hybrid in production, but the only existing test merely
/// `expect()`ed no error, and `hybrid_search_remote` SILENTLY falls back to ANN
/// on failure (`score_type` becomes "cosine"). So a broken BM25 fusion would
/// have shipped unnoticed. Here a token doc is placed FAR in dense space; if
/// hybrid truly fuses, BM25 pulls it into top-3 AND `score_type == "rrf"`. If
/// fs silently fell back to ANN, `score_type` would be "cosine" → this fails,
/// surfacing the latent bug instead of hiding it.
#[tokio::test]
#[ignore]
async fn fs_hybrid_fuses_not_fallback() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = load_embedding_dim();
    store.init_collections(dim).await.expect("init");

    let ws = format!("ws_{}", Uuid::new_v4());
    let mk = |s: f32| -> Vec<f32> { (0..dim).map(|i| s + (i as f32) * 0.001).collect() };
    let chunk = |fid: &str, seed: f32, content: &str| ChunkWithEmbedding {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.clone(),
        file_id: fid.to_string(),
        chunk_index: 0,
        content: content.into(),
        vector: mk(seed),
    };
    let fid_t = Uuid::new_v4().to_string();
    store
        .upsert_chunks_only(&[
            chunk("fv", 0.10, "alpha beta gamma no token"),
            chunk("fd1", 0.11, "filler one"),
            chunk("fd2", 0.12, "filler two"),
            chunk("fd3", 0.13, "filler three"),
            chunk(&fid_t, 0.90, "release notes mention zqxwprodcode here"),
        ])
        .await
        .expect("upsert chunks");

    let req = SearchRequest {
        workspace_id: ws.clone(),
        query: "zqxwprodcode".into(),
        mode: SearchMode::Hybrid,
        limit: 3,
        path_prefix: None,
        query_vector: Some(mk(0.10)),
    };
    let mut hits = Vec::new();
    for _ in 0..15 {
        hits = store.search(&req).await.expect("fs hybrid search");
        if hits.iter().any(|h| h.file_id == fid_t) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    assert!(!hits.is_empty(), "fs hybrid returned no hits");
    assert!(
        hits.iter().all(|h| h.score_type == "rrf"),
        "fs hybrid must report score_type=rrf; \"cosine\" would mean it silently fell back to ANN (F1). got {:?}",
        hits.iter().map(|h| &h.score_type).collect::<Vec<_>>()
    );
    assert!(
        hits.iter().any(|h| h.file_id == fid_t),
        "BM25 fusion must pull the dense-far token chunk into top-3 (else fusion is not happening)"
    );

    // cleanup
    for fid in ["fv", "fd1", "fd2", "fd3"] {
        store.delete_chunks(&ws, fid).await.ok();
    }
    store.delete_chunks(&ws, &fid_t).await.ok();
}

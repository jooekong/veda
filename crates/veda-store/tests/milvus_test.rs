//! Milvus integration tests. Run with: `cargo test -p veda-store -- --ignored`
//! If delete visibility lags, run with `--test-threads=1`.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use uuid::Uuid;
use veda_core::store::{CollectionVectorStore, VectorStore};
use veda_store::MilvusStore;
use veda_types::{
    ChunkWithEmbedding, FieldDefinition, HybridSearchRequest, SearchMode, SearchRequest,
};

#[derive(Debug, Deserialize)]
struct MilvusSection {
    url: String,
    token: Option<String>,
    db: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TestToml {
    milvus: MilvusSection,
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
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg: TestToml = toml::from_str(&raw).expect("parse test.toml [milvus]");
    (cfg.milvus.url, cfg.milvus.token, cfg.milvus.db)
}

#[tokio::test]
#[ignore]
async fn milvus_init_upsert_search_delete() {
    let (url, token, db) = load_milvus();
    let store = MilvusStore::new(&url, token, db);
    let dim = 8u32;
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

    let hy = HybridSearchRequest {
        workspace_id: ws.clone(),
        query_vector: vec.clone(),
        query_text: Some("hello".into()),
        mode: SearchMode::Hybrid,
        limit: 5,
    };
    let _ = store.hybrid_search(&hy).await.expect("hybrid");

    store
        .delete_chunks(&ws, &fid)
        .await
        .expect("delete_chunks");

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
    assert!(gone, "vector rows for file_id should disappear after delete");
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
            embed: false,
        },
        FieldDefinition {
            name: "content".into(),
            field_type: "string".into(),
            index: false,
            embed: true,
        },
    ];

    let dim = 8u32;
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

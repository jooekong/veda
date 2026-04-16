use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use veda_core::store::*;
use veda_sql::VedaSqlEngine;
use veda_types::*;

// ── Mock MetadataStore ──────────────────────────────────

struct MockMeta {
    dentries: Mutex<Vec<Dentry>>,
}

impl MockMeta {
    fn new(dentries: Vec<Dentry>) -> Self {
        Self { dentries: Mutex::new(dentries) }
    }
}

#[async_trait]
impl MetadataStore for MockMeta {
    async fn get_dentry(&self, _ws: &str, path: &str) -> Result<Option<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter().find(|d| d.path == path).cloned())
    }

    async fn list_dentries(&self, _ws: &str, parent: &str) -> Result<Vec<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter()
            .filter(|d| d.parent_path == parent)
            .cloned().collect())
    }

    async fn get_file(&self, _id: &str) -> Result<Option<FileRecord>> { Ok(None) }
    async fn get_file_content(&self, _id: &str) -> Result<Option<String>> { Ok(None) }
    async fn get_file_chunks(&self, _id: &str, _s: Option<i32>, _e: Option<i32>) -> Result<Vec<FileChunk>> { Ok(vec![]) }
    async fn find_file_by_checksum(&self, _ws: &str, _cksum: &str) -> Result<Option<FileRecord>> { Ok(None) }
    async fn get_dentry_path_by_file_id(&self, _ws: &str, _fid: &str) -> Result<Option<String>> { Ok(None) }
    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> { unreachable!() }
}

// ── Mock CollectionMetaStore ──────────────────────────

struct MockCollMeta {
    schemas: Mutex<Vec<CollectionSchema>>,
}

impl MockCollMeta {
    fn new(schemas: Vec<CollectionSchema>) -> Self {
        Self { schemas: Mutex::new(schemas) }
    }
}

#[async_trait]
impl CollectionMetaStore for MockCollMeta {
    async fn create_collection_schema(&self, _s: &CollectionSchema) -> Result<()> { Ok(()) }
    async fn get_collection_schema(&self, _ws: &str, _name: &str) -> Result<Option<CollectionSchema>> { Ok(None) }
    async fn get_collection_schema_by_id(&self, _id: &str) -> Result<Option<CollectionSchema>> { Ok(None) }
    async fn list_collection_schemas(&self, _ws: &str) -> Result<Vec<CollectionSchema>> {
        Ok(self.schemas.lock().unwrap().clone())
    }
    async fn delete_collection_schema(&self, _id: &str) -> Result<()> { Ok(()) }
}

// ── Mock CollectionVectorStore ────────────────────────

struct MockCollVector {
    data: Mutex<HashMap<String, Vec<serde_json::Value>>>,
}

impl MockCollVector {
    fn new() -> Self {
        Self { data: Mutex::new(HashMap::new()) }
    }
    fn insert(&self, name: &str, rows: Vec<serde_json::Value>) {
        self.data.lock().unwrap().insert(name.to_string(), rows);
    }
}

#[async_trait]
impl CollectionVectorStore for MockCollVector {
    async fn create_dynamic_collection(&self, _n: &str, _f: &[FieldDefinition], _d: u32) -> Result<()> { Ok(()) }
    async fn drop_dynamic_collection(&self, _n: &str) -> Result<()> { Ok(()) }
    async fn insert_collection_rows(&self, _n: &str, _ws: &str, _r: &[serde_json::Value]) -> Result<()> { Ok(()) }
    async fn search_collection(&self, name: &str, _ws: &str, _vec: &[f32], _limit: usize) -> Result<Vec<serde_json::Value>> {
        Ok(self.data.lock().unwrap().get(name).cloned().unwrap_or_default())
    }
}

// ── Mock EmbeddingService ─────────────────────────────

struct MockEmbed;

#[async_trait]
impl EmbeddingService for MockEmbed {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0; 128]).collect())
    }
    fn dimension(&self) -> usize { 128 }
}

// ── Helpers ───────────────────────────────────────────

fn make_dentry(path: &str, name: &str, is_dir: bool) -> Dentry {
    let parent = if path == "/" { "".to_string() } else {
        let p = path.rsplit_once('/').map(|(a, _)| a).unwrap_or("");
        if p.is_empty() { "/".to_string() } else { p.to_string() }
    };
    Dentry {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: "ws1".into(),
        parent_path: parent,
        name: name.into(),
        path: path.into(),
        file_id: if is_dir { None } else { Some(uuid::Uuid::new_v4().to_string()) },
        is_dir,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn make_engine(
    dentries: Vec<Dentry>,
    schemas: Vec<CollectionSchema>,
    coll_vector: Arc<MockCollVector>,
) -> VedaSqlEngine {
    VedaSqlEngine::new(
        Arc::new(MockMeta::new(dentries)),
        Arc::new(MockCollMeta::new(schemas)),
        coll_vector,
        Arc::new(MockEmbed),
    )
}

// ── Tests ─────────────────────────────────────────────

#[tokio::test]
async fn select_star_from_files() {
    let dentries = vec![
        make_dentry("/readme.md", "readme.md", false),
        make_dentry("/src", "src", true),
    ];
    let engine = make_engine(dentries, vec![], Arc::new(MockCollVector::new()));
    let batches = engine.execute("ws1", "SELECT path, name, is_dir FROM files").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn where_filter_on_files() {
    let dentries = vec![
        make_dentry("/a.txt", "a.txt", false),
        make_dentry("/b.txt", "b.txt", false),
        make_dentry("/docs", "docs", true),
    ];
    let engine = make_engine(dentries, vec![], Arc::new(MockCollVector::new()));
    let batches = engine.execute("ws1", "SELECT path FROM files WHERE is_dir = false").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn count_files() {
    let dentries = vec![
        make_dentry("/a", "a", false),
        make_dentry("/b", "b", false),
        make_dentry("/c", "c", false),
    ];
    let engine = make_engine(dentries, vec![], Arc::new(MockCollVector::new()));
    let batches = engine.execute("ws1", "SELECT count(*) as cnt FROM files").await.unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 1);
    let col = batch.column(0);
    let arr = col.as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
    assert_eq!(arr.value(0), 3);
}

#[tokio::test]
async fn collection_query() {
    let coll_vec = Arc::new(MockCollVector::new());
    coll_vec.insert("veda_coll_s1", vec![
        serde_json::json!({"id": "1", "title": "Widget", "price": "9.99"}),
        serde_json::json!({"id": "2", "title": "Gadget", "price": "19.99"}),
    ]);

    let schema = CollectionSchema {
        id: "s1".into(),
        workspace_id: "ws1".into(),
        name: "products".into(),
        collection_type: CollectionType::Structured,
        schema_json: serde_json::json!([
            {"name": "title", "field_type": "string", "index": false, "embed": true},
            {"name": "price", "field_type": "string", "index": false, "embed": false}
        ]),
        embedding_source: Some("title".into()),
        embedding_dim: Some(128),
        status: CollectionStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let engine = make_engine(vec![], vec![schema], coll_vec);
    let batches = engine.execute("ws1", "SELECT id, title FROM products").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn collection_where_filter() {
    let coll_vec = Arc::new(MockCollVector::new());
    coll_vec.insert("veda_coll_s2", vec![
        serde_json::json!({"id": "1", "name": "Apple", "category": "fruit"}),
        serde_json::json!({"id": "2", "name": "Carrot", "category": "vegetable"}),
        serde_json::json!({"id": "3", "name": "Banana", "category": "fruit"}),
    ]);

    let schema = CollectionSchema {
        id: "s2".into(),
        workspace_id: "ws1".into(),
        name: "items".into(),
        collection_type: CollectionType::Structured,
        schema_json: serde_json::json!([
            {"name": "name", "field_type": "string", "index": false, "embed": false},
            {"name": "category", "field_type": "string", "index": true, "embed": false}
        ]),
        embedding_source: None,
        embedding_dim: Some(128),
        status: CollectionStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let engine = make_engine(vec![], vec![schema], coll_vec);
    let batches = engine.execute("ws1", "SELECT name FROM items WHERE category = 'fruit'").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn empty_workspace_returns_empty() {
    let engine = make_engine(vec![], vec![], Arc::new(MockCollVector::new()));
    let batches = engine.execute("ws1", "SELECT * FROM files").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn invalid_sql_returns_error() {
    let engine = make_engine(vec![], vec![], Arc::new(MockCollVector::new()));
    let result = engine.execute("ws1", "SELEKT * FRUM nowhere").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn files_with_nested_dirs() {
    let dentries = vec![
        make_dentry("/src", "src", true),
        make_dentry("/src/main.rs", "main.rs", false),
        make_dentry("/src/lib.rs", "lib.rs", false),
    ];
    let engine = make_engine(dentries, vec![], Arc::new(MockCollVector::new()));
    let batches = engine.execute("ws1", "SELECT path FROM files WHERE is_dir = false").await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);
}

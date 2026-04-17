use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use veda_core::store::*;
use veda_sql::VedaSqlEngine;
use veda_types::*;

// Re-usable MockMeta that also supports file content (for UDF tests)
struct MockMetaFull {
    dentries: Arc<Mutex<Vec<Dentry>>>,
    files: Arc<Mutex<Vec<FileRecord>>>,
    file_contents: Arc<Mutex<HashMap<String, String>>>,
    fs_events: Arc<Mutex<Vec<FsEvent>>>,
}

impl MockMetaFull {
    fn new() -> Self {
        Self {
            dentries: Arc::new(Mutex::new(vec![])),
            files: Arc::new(Mutex::new(vec![])),
            file_contents: Arc::new(Mutex::new(HashMap::new())),
            fs_events: Arc::new(Mutex::new(vec![])),
        }
    }
}

#[async_trait]
impl MetadataStore for MockMetaFull {
    async fn get_dentry(&self, ws: &str, path: &str) -> Result<Option<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter().find(|d| d.workspace_id == ws && d.path == path).cloned())
    }
    async fn list_dentries(&self, ws: &str, parent: &str) -> Result<Vec<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter().filter(|d| d.workspace_id == ws && d.parent_path == parent).cloned().collect())
    }
    async fn get_file(&self, id: &str) -> Result<Option<FileRecord>> {
        Ok(self.files.lock().unwrap().iter().find(|f| f.id == id).cloned())
    }
    async fn get_file_content(&self, id: &str) -> Result<Option<String>> {
        Ok(self.file_contents.lock().unwrap().get(id).cloned())
    }
    async fn get_file_chunks(&self, _id: &str, _s: Option<i32>, _e: Option<i32>) -> Result<Vec<FileChunk>> { Ok(vec![]) }
    async fn find_file_by_checksum(&self, _ws: &str, _cksum: &str) -> Result<Option<FileRecord>> { Ok(None) }
    async fn get_dentry_path_by_file_id(&self, _ws: &str, _fid: &str) -> Result<Option<String>> { Ok(None) }
    async fn query_fs_events(&self, ws: &str, since: i64, _prefix: Option<&str>, limit: usize) -> Result<Vec<FsEvent>> {
        let st = self.fs_events.lock().unwrap();
        let mut v: Vec<_> = st.iter().filter(|e| e.workspace_id == ws && e.id > since).cloned().collect();
        v.sort_by_key(|e| e.id);
        v.truncate(limit);
        Ok(v)
    }
    async fn storage_stats(&self, ws: &str) -> Result<StorageStats> {
        let dentries = self.dentries.lock().unwrap();
        let files = self.files.lock().unwrap();
        let total_files = dentries.iter().filter(|d| d.workspace_id == ws && !d.is_dir).count() as i64;
        let total_dirs = dentries.iter().filter(|d| d.workspace_id == ws && d.is_dir).count() as i64;
        let total_bytes: i64 = files.iter().filter(|f| f.workspace_id == ws).map(|f| f.size_bytes).sum();
        Ok(StorageStats { total_files, total_directories: total_dirs, total_bytes })
    }
    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> {
        Ok(Box::new(MockMetaFullTx {
            dentries: self.dentries.clone(),
            files: self.files.clone(),
            file_contents: self.file_contents.clone(),
            fs_events: self.fs_events.clone(),
        }))
    }
}

// Tx operates directly on shared state (no isolation, fine for tests)
struct MockMetaFullTx {
    dentries: Arc<Mutex<Vec<Dentry>>>,
    files: Arc<Mutex<Vec<FileRecord>>>,
    file_contents: Arc<Mutex<HashMap<String, String>>>,
    fs_events: Arc<Mutex<Vec<FsEvent>>>,
}

#[async_trait]
impl MetadataTx for MockMetaFullTx {
    async fn get_dentry(&mut self, ws: &str, path: &str) -> Result<Option<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter().find(|d| d.workspace_id == ws && d.path == path).cloned())
    }
    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()> { self.dentries.lock().unwrap().push(dentry.clone()); Ok(()) }
    async fn update_dentry_file_id(&mut self, ws: &str, path: &str, file_id: &str) -> Result<()> {
        if let Some(d) = self.dentries.lock().unwrap().iter_mut().find(|d| d.workspace_id == ws && d.path == path) { d.file_id = Some(file_id.to_string()); }
        Ok(())
    }
    async fn delete_dentry(&mut self, ws: &str, path: &str) -> Result<u64> {
        let mut st = self.dentries.lock().unwrap();
        let before = st.len();
        st.retain(|d| !(d.workspace_id == ws && d.path == path));
        Ok((before - st.len()) as u64)
    }
    async fn list_dentries_under(&mut self, ws: &str, prefix: &str) -> Result<Vec<Dentry>> {
        Ok(self.dentries.lock().unwrap().iter().filter(|d| d.workspace_id == ws && d.path.starts_with(&format!("{prefix}/"))).cloned().collect())
    }
    async fn delete_dentries_under(&mut self, ws: &str, parent: &str) -> Result<u64> {
        let mut st = self.dentries.lock().unwrap();
        let before = st.len();
        st.retain(|d| !(d.workspace_id == ws && d.path.starts_with(&format!("{parent}/"))));
        Ok((before - st.len()) as u64)
    }
    async fn rename_dentry(&mut self, ws: &str, old: &str, new: &str, np: &str, nn: &str) -> Result<()> {
        if let Some(d) = self.dentries.lock().unwrap().iter_mut().find(|d| d.workspace_id == ws && d.path == old) {
            d.path = new.to_string(); d.parent_path = np.to_string(); d.name = nn.to_string();
        }
        Ok(())
    }
    async fn get_file(&mut self, id: &str) -> Result<Option<FileRecord>> { Ok(self.files.lock().unwrap().iter().find(|f| f.id == id).cloned()) }
    async fn insert_file(&mut self, file: &FileRecord) -> Result<()> { self.files.lock().unwrap().push(file.clone()); Ok(()) }
    async fn update_file_revision(&mut self, id: &str, rev: i32, size: i64, cksum: &str, lc: Option<i32>, st: StorageType) -> Result<()> {
        if let Some(f) = self.files.lock().unwrap().iter_mut().find(|f| f.id == id) { f.revision = rev; f.size_bytes = size; f.checksum_sha256 = cksum.to_string(); f.line_count = lc; f.storage_type = st; }
        Ok(())
    }
    async fn decrement_ref_count(&mut self, id: &str) -> Result<i32> {
        if let Some(f) = self.files.lock().unwrap().iter_mut().find(|f| f.id == id) { f.ref_count -= 1; return Ok(f.ref_count); }
        Ok(0)
    }
    async fn increment_ref_count(&mut self, id: &str) -> Result<()> {
        if let Some(f) = self.files.lock().unwrap().iter_mut().find(|f| f.id == id) { f.ref_count += 1; }
        Ok(())
    }
    async fn delete_file(&mut self, id: &str) -> Result<()> { self.files.lock().unwrap().retain(|f| f.id != id); Ok(()) }
    async fn insert_file_content(&mut self, id: &str, content: &str) -> Result<()> { self.file_contents.lock().unwrap().insert(id.to_string(), content.to_string()); Ok(()) }
    async fn delete_file_content(&mut self, id: &str) -> Result<()> { self.file_contents.lock().unwrap().remove(id); Ok(()) }
    async fn insert_file_chunks(&mut self, _chunks: &[FileChunk]) -> Result<()> { Ok(()) }
    async fn delete_file_chunks(&mut self, _id: &str) -> Result<()> { Ok(()) }
    async fn insert_outbox(&mut self, _event: &OutboxEvent) -> Result<()> { Ok(()) }
    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> { self.fs_events.lock().unwrap().push(event.clone()); Ok(()) }
    async fn commit(self: Box<Self>) -> Result<()> { Ok(()) }
    async fn rollback(self: Box<Self>) -> Result<()> { Ok(()) }
}

fn make_full_engine(meta: Arc<MockMetaFull>) -> VedaSqlEngine {
    let meta_store: Arc<dyn MetadataStore> = meta;
    let fs_service = Arc::new(veda_core::service::fs::FsService::new(meta_store.clone()));
    VedaSqlEngine::new(
        meta_store,
        Arc::new(MockCollMeta::new(vec![])),
        Arc::new(MockCollVector::new()),
        Arc::new(MockEmbed),
        fs_service,
    )
}

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
    async fn query_fs_events(&self, _ws: &str, _since: i64, _prefix: Option<&str>, _limit: usize) -> Result<Vec<FsEvent>> { Ok(vec![]) }
    async fn storage_stats(&self, _ws: &str) -> Result<StorageStats> { Ok(StorageStats { total_files: 0, total_directories: 0, total_bytes: 0 }) }
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
    async fn query_collection(&self, name: &str, _ws: &str, _limit: usize) -> Result<Vec<serde_json::Value>> {
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
    let meta: Arc<dyn MetadataStore> = Arc::new(MockMeta::new(dentries));
    let fs_service = Arc::new(veda_core::service::fs::FsService::new(meta.clone()));
    VedaSqlEngine::new(
        meta,
        Arc::new(MockCollMeta::new(schemas)),
        coll_vector,
        Arc::new(MockEmbed),
        fs_service,
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

// ── UDF Tests ─────────────────────────────────────────

#[tokio::test]
async fn udf_veda_write_and_read() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    let batches = engine
        .execute("ws1", "SELECT veda_write('/hello.txt', 'world') as bytes_written")
        .await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
    assert_eq!(arr.value(0), 5);

    let batches = engine
        .execute("ws1", "SELECT veda_read('/hello.txt') as content")
        .await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert_eq!(arr.value(0), "world");
}

#[tokio::test]
async fn udf_veda_exists() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    let batches = engine.execute("ws1", "SELECT veda_exists('/nope.txt') as e").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(!arr.value(0));

    engine.execute("ws1", "SELECT veda_write('/exists.txt', 'hi')").await.unwrap();

    let batches = engine.execute("ws1", "SELECT veda_exists('/exists.txt') as e").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(arr.value(0));
}

#[tokio::test]
async fn udf_veda_size_and_mtime() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);
    engine.execute("ws1", "SELECT veda_write('/data.txt', 'hello world')").await.unwrap();

    let batches = engine.execute("ws1", "SELECT veda_size('/data.txt') as sz").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
    assert_eq!(arr.value(0), 11);

    let batches = engine.execute("ws1", "SELECT veda_mtime('/data.txt') as mt").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert!(!arr.value(0).is_empty());
}

#[tokio::test]
async fn udf_veda_append() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);
    engine.execute("ws1", "SELECT veda_write('/log.txt', 'line1\n')").await.unwrap();
    engine.execute("ws1", "SELECT veda_append('/log.txt', 'line2\n')").await.unwrap();

    let batches = engine.execute("ws1", "SELECT veda_read('/log.txt') as c").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert_eq!(arr.value(0), "line1\nline2\n");
}

#[tokio::test]
async fn udf_veda_mkdir_and_remove() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    let batches = engine.execute("ws1", "SELECT veda_mkdir('/mydir') as ok").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(arr.value(0));

    let batches = engine.execute("ws1", "SELECT veda_exists('/mydir') as e").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(arr.value(0));

    engine.execute("ws1", "SELECT veda_write('/target.txt', 'data')").await.unwrap();
    let batches = engine.execute("ws1", "SELECT veda_remove('/target.txt') as r").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
    assert_eq!(arr.value(0), 1);

    let batches = engine.execute("ws1", "SELECT veda_exists('/target.txt') as e").await.unwrap();
    let arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(!arr.value(0));
}

#[tokio::test]
async fn udf_column_arg_veda_exists() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    engine.execute("ws1", "SELECT veda_write('/a.txt', 'aaa')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/b.txt', 'bbb')").await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT path, veda_exists(path) as e FROM files WHERE is_dir = false ORDER BY path")
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);

    let arr = batches[0].column(1).as_any().downcast_ref::<arrow::array::BooleanArray>().unwrap();
    assert!(arr.value(0), "first row should exist");
    assert!(arr.value(1), "second row should exist");
}

#[tokio::test]
async fn udf_column_arg_veda_read() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    engine.execute("ws1", "SELECT veda_write('/x.txt', 'content_x')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/y.txt', 'content_y')").await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT path, veda_read(path) as content FROM files WHERE is_dir = false ORDER BY path")
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);

    let arr = batches[0].column(1).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert_eq!(arr.value(0), "content_x");
    assert_eq!(arr.value(1), "content_y");
}

// ── veda_fs() Table Function Tests ────────────────────

#[tokio::test]
async fn veda_fs_dir_listing() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    engine.execute("ws1", "SELECT veda_mkdir('/docs')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/docs/a.txt', 'aaa')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/docs/b.md', 'bbb')").await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT path, name, type, size_bytes FROM veda_fs('/docs/')")
        .await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2, "should list 2 files under /docs/");
}

#[tokio::test]
async fn veda_fs_read_plain_text() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    engine.execute("ws1", "SELECT veda_write('/notes.txt', 'line1\nline2\nline3')").await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT _line_number, line FROM veda_fs('/notes.txt')")
        .await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 3);

    let ln_arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
    let line_arr = batches[0].column(1).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert_eq!(ln_arr.value(0), 1);
    assert_eq!(line_arr.value(0), "line1");
    assert_eq!(line_arr.value(2), "line3");
}

#[tokio::test]
async fn veda_fs_read_csv() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    let csv = "name,age\nAlice,30\nBob,25";
    engine.execute("ws1", &format!("SELECT veda_write('/data.csv', '{csv}')")).await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT name, age FROM veda_fs('/data.csv')")
        .await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);

    let name_arr = batches[0].column(0).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert_eq!(name_arr.value(0), "Alice");
    assert_eq!(name_arr.value(1), "Bob");
}

#[tokio::test]
async fn veda_fs_read_jsonl() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    let jsonl = r#"{"level":"info","msg":"start"}
{"level":"error","msg":"fail"}"#;
    engine.execute("ws1", &format!("SELECT veda_write('/app.jsonl', '{jsonl}')")).await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT _line_number, line FROM veda_fs('/app.jsonl')")
        .await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 2);

    let line_arr = batches[0].column(1).as_any().downcast_ref::<arrow::array::StringArray>().unwrap();
    assert!(line_arr.value(0).contains("info"));
    assert!(line_arr.value(1).contains("error"));
}

#[tokio::test]
async fn veda_fs_glob() {
    let meta = Arc::new(MockMetaFull::new());
    let engine = make_full_engine(meta);

    engine.execute("ws1", "SELECT veda_mkdir('/logs')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/logs/a.txt', 'log_a\nline2')").await.unwrap();
    engine.execute("ws1", "SELECT veda_write('/logs/b.txt', 'log_b')").await.unwrap();

    let batches = engine
        .execute("ws1", "SELECT _line_number, line, _path FROM veda_fs('/logs/*.txt')")
        .await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 3, "2 lines from a.txt + 1 from b.txt");
}

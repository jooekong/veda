//! `SearchService::workspace_map` assembly.
//!
//! The map is the workspace's root-level view. The root has no dentry and
//! therefore no summary row of its own, so the view is built from the
//! top-level children — these tests pin the assembly rules that makes
//! usable: ordering, the read cap, per-entry field selection, and what
//! `summary_state` is allowed to claim.

mod mock_store;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, VectorStore};
use veda_types::api::MapSummaryState;
use veda_types::*;

const CAP: usize = 200;

struct NoEmbedding;

#[async_trait]
impl EmbeddingService for NoEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0]).collect())
    }
    fn dimension(&self) -> usize {
        1
    }
}

struct NoVector;

#[async_trait]
impl VectorStore for NoVector {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn upsert_chunks(&self, _c: &[ChunkWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_chunks(&self, _ws: &str, _fid: &str) -> Result<()> {
        Ok(())
    }
    async fn search(&self, _r: &SearchRequest) -> Result<Vec<SearchHit>> {
        Ok(vec![])
    }
    async fn upsert_summaries(&self, _s: &[SummaryWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_summary(&self, _ws: &str, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_summaries(&self, _r: &SearchRequest) -> Result<Vec<SearchHit>> {
        Ok(vec![])
    }
    async fn list_summary_ids(&self, _ws: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn list_chunk_file_ids(&self, _ws: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn init_collections(&self, _dim: u32) -> Result<()> {
        Ok(())
    }
}

fn dentry(id: &str, name: &str, is_dir: bool, file_id: Option<&str>) -> Dentry {
    Dentry {
        id: id.into(),
        workspace_id: "ws1".into(),
        parent_path: "/".into(),
        name: name.into(),
        path: format!("/{name}"),
        file_id: file_id.map(Into::into),
        is_dir,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn file(id: &str, size: i64) -> FileRecord {
    FileRecord {
        id: id.into(),
        workspace_id: "ws1".into(),
        size_bytes: size,
        mime_type: "text/plain".into(),
        storage_type: StorageType::Inline,
        source_type: SourceType::Text,
        line_count: Some(1),
        checksum_sha256: "sha".into(),
        revision: 1,
        ref_count: 1,
        last_embedded_content_hash: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn summary(l0: &str) -> FileSummary {
    FileSummary {
        id: "s".into(),
        workspace_id: "ws1".into(),
        file_id: None,
        dentry_id: None,
        l0_abstract: l0.into(),
        l1_overview: String::new(),
        status: SummaryStatus::Ready,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Builds a service over a mock store the caller has already populated.
fn service(meta: Arc<mock_store::MockMetadataStore>) -> SearchService {
    SearchService::new(meta, Arc::new(NoVector), Arc::new(NoEmbedding))
}

fn store() -> Arc<mock_store::MockMetadataStore> {
    Arc::new(mock_store::MockMetadataStore::new())
}

#[tokio::test]
async fn entries_list_directories_before_files() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dentry("d-z", "zebra", true, None),
            dentry("d-a", "alpha", true, None),
            dentry("f-b", "b.md", false, Some("fb")),
            dentry("f-a", "a.md", false, Some("fa")),
        ];
        st.files = vec![file("fa", 1), file("fb", 2)];
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();
    let paths: Vec<&str> = map.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["/alpha", "/zebra", "/a.md", "/b.md"]);
    assert!(!map.truncated);
}

/// The cap must reach the query. Asserting only on the returned length
/// would also pass for an implementation that loads every root entry and
/// truncates in memory — precisely the shape that OOMed a previous
/// listing path on large workspaces.
#[tokio::test]
async fn read_cap_is_pushed_into_the_query() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = (0..250)
            .map(|i| dentry(&format!("d{i}"), &format!("dir{i:03}"), true, None))
            .collect();
    }
    let map = service(Arc::clone(&meta)).workspace_map("ws1", CAP).await.unwrap();

    assert_eq!(map.entries.len(), CAP);
    assert!(map.truncated);
    // CAP + 1: one extra row is how "is there more?" is answered without a
    // second COUNT query.
    assert_eq!(meta.state.lock().unwrap().children_capped_limits, vec![CAP + 1]);
}

/// The batched lookups must be built from the *truncated* entry set. If they
/// were built before the cut, a workspace with 50k loose root files would
/// send a 50k-placeholder `IN (...)` — the same load-everything shape the
/// cap exists to prevent, just moved one step later.
#[tokio::test]
async fn batch_lookups_are_bounded_by_the_cap() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        // 250 files and 250 directories: well past the cap on both sides,
        // so neither the file batch nor the directory batch can sneak past.
        for i in 0..250 {
            st.dentries
                .push(dentry(&format!("d{i}"), &format!("dir{i:03}"), true, None));
            let fid = format!("f{i}");
            st.dentries.push(dentry(
                &format!("fd{i}"),
                &format!("file{i:03}.md"),
                false,
                Some(&fid),
            ));
            st.files.push(file(&fid, 1));
        }
    }
    let map = service(Arc::clone(&meta)).workspace_map("ws1", CAP).await.unwrap();
    assert_eq!(map.entries.len(), CAP);
    assert!(map.truncated);

    for (label, n) in &meta.state.lock().unwrap().batch_id_counts {
        assert!(
            *n <= CAP,
            "{label} was handed {n} ids, above the {CAP} cap"
        );
    }
    // All 200 surviving entries are directories, so the directory batch is
    // full and the file batch is empty — the cut lands exactly there.
    let counts = meta.state.lock().unwrap().batch_id_counts.clone();
    assert!(counts.contains(&("summaries_by_dentry", CAP)), "{counts:?}");
    assert!(counts.contains(&("files_batch", 0)), "{counts:?}");
}

#[tokio::test]
async fn summary_state_is_ready_when_every_entry_has_an_abstract() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dentry("d1", "docs", true, None),
            dentry("f1", "a.md", false, Some("fa")),
        ];
        st.files = vec![file("fa", 10)];
        st.dir_summaries.insert("d1".into(), summary("the docs"));
        st.file_summaries.insert("fa".into(), summary("a file"));
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();
    assert_eq!(map.summary_state, MapSummaryState::Ready);
    assert_eq!(map.entries[0].l0_abstract.as_deref(), Some("the docs"));
    assert_eq!(map.entries[1].l0_abstract.as_deref(), Some("a file"));
}

#[tokio::test]
async fn missing_abstracts_yield_partial_and_are_omitted_from_json() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dentry("d1", "docs", true, None),
            dentry("d2", "wiki", true, None),
        ];
        st.dir_summaries.insert("d1".into(), summary("the docs"));
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();
    assert_eq!(map.summary_state, MapSummaryState::Partial);

    // Absent, not null: a client checking `"abstract" in entry` must not
    // see a key it then has to null-check.
    let v = serde_json::to_value(&map).unwrap();
    assert!(v["entries"][0].get("abstract").is_some());
    assert!(v["entries"][1].get("abstract").is_none());
}

/// Truncated-away entries must not drag the state down: coverage describes
/// what was returned, not the whole workspace.
#[tokio::test]
async fn summary_state_covers_returned_entries_only() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = (0..250)
            .map(|i| dentry(&format!("d{i}"), &format!("dir{i:03}"), true, None))
            .collect();
        // Only the 200 that survive the cap (dir000..dir199) get summaries.
        for i in 0..CAP {
            st.dir_summaries
                .insert(format!("d{i}"), summary("covered"));
        }
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();
    assert_eq!(map.entries.len(), CAP);
    assert!(map.truncated);
    assert_eq!(map.summary_state, MapSummaryState::Ready);
}

/// `file_count` is keyed off is_dir, never off "the counts map happens to
/// have this name". A root-level file groups under its own file name in
/// that map, so a name-keyed lookup would report a bogus count for it.
#[tokio::test]
async fn file_count_is_for_directories_and_size_for_files() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dentry("d1", "docs", true, None),
            dentry("f1", "README.md", false, Some("fr")),
        ];
        st.files = vec![file("fr", 4096)];
        st.top_level_counts.insert("docs".into(), 42);
        // A root-level file legitimately appears in the counts map keyed by
        // its own name — the entry must still not carry a file_count.
        st.top_level_counts.insert("README.md".into(), 1);
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();

    let dir = &map.entries[0];
    assert_eq!(dir.file_count, Some(42));
    assert_eq!(dir.size_bytes, None);

    let f = &map.entries[1];
    assert_eq!(f.file_count, None, "a file must never report a file_count");
    assert_eq!(f.size_bytes, Some(4096));
}

/// A directory with no counted files reports 0 rather than omitting the
/// field — "empty" and "unknown" are different answers for an agent
/// deciding whether to descend.
#[tokio::test]
async fn directory_with_no_files_reports_zero_not_absent() {
    let meta = store();
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![dentry("d1", "empty", true, None)];
    }
    let map = service(meta).workspace_map("ws1", CAP).await.unwrap();
    assert_eq!(map.entries[0].file_count, Some(0));
}

#[tokio::test]
async fn empty_workspace_returns_an_empty_map_not_an_error() {
    let map = service(store()).workspace_map("ws1", CAP).await.unwrap();
    assert!(map.entries.is_empty());
    assert!(!map.truncated);
    assert_eq!(map.stats.total_files, 0);
    // Vacuously complete: no entry is missing an abstract.
    assert_eq!(map.summary_state, MapSummaryState::Ready);
}

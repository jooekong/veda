mod mock_store;

use std::sync::Arc;

use async_trait::async_trait;
use veda_core::service::search::SearchService;
use veda_core::store::{EmbeddingService, VectorStore};
use veda_types::*;

struct MockEmbedding;

#[async_trait]
impl EmbeddingService for MockEmbedding {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }
    fn dimension(&self) -> usize {
        3
    }
}

struct MockVector {
    chunk_hits: Vec<SearchHit>,
    summary_hits: Vec<SearchHit>,
}

#[async_trait]
impl VectorStore for MockVector {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn upsert_chunks(&self, _chunks: &[ChunkWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_chunks(&self, _ws: &str, _fid: &str) -> Result<()> {
        Ok(())
    }
    async fn search(&self, _req: &SearchRequest) -> Result<Vec<SearchHit>> {
        Ok(self.chunk_hits.clone())
    }
    async fn upsert_summaries(&self, _summaries: &[SummaryWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_summary(&self, _ws: &str, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_summaries(&self, _req: &SearchRequest) -> Result<Vec<SearchHit>> {
        Ok(self.summary_hits.clone())
    }
    async fn list_chunk_file_ids(&self, _ws: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn list_summary_ids(&self, _ws: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn init_collections(&self, _dim: u32) -> Result<()> {
        Ok(())
    }
}

fn make_service(chunk_hits: Vec<SearchHit>, summary_hits: Vec<SearchHit>) -> SearchService {
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    let vector = Arc::new(MockVector {
        chunk_hits,
        summary_hits,
    });
    let emb = Arc::new(MockEmbedding);
    SearchService::new(meta, vector, emb)
}

#[tokio::test]
async fn search_full_returns_chunks() {
    let chunk_hits = vec![SearchHit {
        file_id: "f1".into(),
        dentry_id: None,
        chunk_index: Some(0),
        content: "chunk content".into(),
        score: 0.9,
        score_type: "cosine".into(),
        path: Some("/a.md".into()),
        l0_abstract: None,
        l1_overview: None,
    }];
    let svc = make_service(chunk_hits, vec![]);

    let hits = svc
        .search(
            "ws1",
            "test query",
            SearchMode::Hybrid,
            10,
            None,
            DetailLevel::Full,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].content, "chunk content");
    assert!(hits[0].l0_abstract.is_none());
}

#[tokio::test]
async fn search_abstract_returns_summaries() {
    let summary_hits = vec![SearchHit {
        file_id: "f1".into(),
        dentry_id: None,
        chunk_index: None,
        content: "L0 abstract text".into(),
        score: 0.95,
        score_type: "cosine".into(),
        path: Some("/docs/readme.md".into()),
        l0_abstract: Some("L0 abstract text".into()),
        l1_overview: None,
    }];
    let svc = make_service(vec![], summary_hits);

    let hits = svc
        .search(
            "ws1",
            "test query",
            SearchMode::Semantic,
            10,
            None,
            DetailLevel::Abstract,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].l0_abstract.is_some());
    assert_eq!(hits[0].l0_abstract.as_deref(), Some("L0 abstract text"));
}

#[tokio::test]
async fn search_with_path_prefix_filters() {
    let summary_hits = vec![
        SearchHit {
            file_id: "f1".into(),
            dentry_id: None,
            chunk_index: None,
            content: "in docs".into(),
            score: 0.9,
            score_type: "cosine".into(),
            path: Some("/docs/a.md".into()),
            l0_abstract: Some("in docs".into()),
            l1_overview: None,
        },
        SearchHit {
            file_id: "f2".into(),
            dentry_id: None,
            chunk_index: None,
            content: "in src".into(),
            score: 0.8,
            score_type: "cosine".into(),
            path: Some("/src/b.rs".into()),
            l0_abstract: Some("in src".into()),
            l1_overview: None,
        },
    ];
    let svc = make_service(vec![], summary_hits);

    let hits = svc
        .search(
            "ws1",
            "test",
            SearchMode::Semantic,
            10,
            Some("/docs"),
            DetailLevel::Abstract,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path.as_deref(), Some("/docs/a.md"));
}

// ── Search-hit counting (review 2026-08-05 must-tests) ─────────────

/// Service wired to an ENABLED recorder. Fixture hits carry `path: None`
/// so they go through the real `resolve_paths` batch (that's where
/// `dentry_id` gets populated — hits arriving with a path never would in
/// production either, since Milvus rows always start with `path: None`).
fn make_counting_service(
    chunk_hits: Vec<SearchHit>,
    dentries: Vec<Dentry>,
) -> (
    SearchService,
    Arc<veda_core::service::access_stats::AccessRecorder>,
    Arc<std::sync::Mutex<mock_store::MockState>>,
) {
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    let state = meta.state.clone();
    state.lock().unwrap().dentries = dentries;
    let recorder = Arc::new(veda_core::service::access_stats::AccessRecorder::new(
        meta.clone(),
        8,
        true,
    ));
    let vector = Arc::new(MockVector {
        chunk_hits,
        summary_hits: vec![],
    });
    let svc = SearchService::with_stats(meta, vector, Arc::new(MockEmbedding), recorder.clone());
    (svc, recorder, state)
}

fn chunk_hit(file_id: &str, idx: i32) -> SearchHit {
    SearchHit {
        file_id: file_id.into(),
        dentry_id: None,
        chunk_index: Some(idx),
        content: format!("chunk {idx}"),
        score: 0.9,
        score_type: "cosine".into(),
        path: None,
        l0_abstract: None,
        l1_overview: None,
    }
}

fn dentry(id: &str, ws: &str, path: &str, file_id: &str) -> Dentry {
    Dentry {
        id: id.into(),
        workspace_id: ws.into(),
        path: path.into(),
        parent_path: "/".into(),
        name: path.trim_start_matches('/').into(),
        is_dir: false,
        file_id: Some(file_id.into()),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn search_hit_counting_dedupes_chunks_per_file() {
    // Three chunks of one file + one chunk of another: 2 impressions, not 4.
    let hits = vec![
        chunk_hit("f1", 0),
        chunk_hit("f1", 1),
        chunk_hit("f1", 2),
        chunk_hit("f2", 0),
    ];
    let dentries = vec![
        dentry("d1", "ws1", "/a.md", "f1"),
        dentry("d2", "ws1", "/b.md", "f2"),
    ];
    let (svc, recorder, state) = make_counting_service(hits, dentries);
    svc.search("ws1", "q", SearchMode::Hybrid, 10, None, DetailLevel::Full)
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    let rows = state.lock().unwrap().doc_access_rows.clone();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.search_hits, 1, "per-query dedup: one impression per file");
        assert_eq!(r.reads, 0);
    }
}

#[tokio::test]
async fn unresolvable_hits_are_not_counted() {
    // A hit whose file_id has no live dentry (detached file, or a
    // directory-summary hit whose `file_id` is actually a dentry_id) must
    // be skipped — never counted under a fabricated key.
    let hits = vec![chunk_hit("f1", 0), chunk_hit("ghost", 0)];
    let dentries = vec![dentry("d1", "ws1", "/a.md", "f1")];
    let (svc, recorder, state) = make_counting_service(hits, dentries);
    svc.search("ws1", "q", SearchMode::Hybrid, 10, None, DetailLevel::Full)
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    let rows = state.lock().unwrap().doc_access_rows.clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dentry_id, "d1");
}

#[tokio::test]
async fn copy_alias_attribution_is_deterministic_smallest_path() {
    // One file_id behind two dentries (copy_file alias). Attribution must
    // go to the smallest path — pinned so the mock mirrors the MySQL
    // `ORDER BY path, id` contract on DentryPathRef.
    let hits = vec![chunk_hit("f1", 0)];
    let dentries = vec![
        dentry("d-z", "ws1", "/z-copy.md", "f1"),
        dentry("d-a", "ws1", "/a-orig.md", "f1"),
    ];
    let (svc, recorder, state) = make_counting_service(hits, dentries);
    let out = svc
        .search("ws1", "q", SearchMode::Hybrid, 10, None, DetailLevel::Full)
        .await
        .unwrap();
    assert_eq!(out[0].path.as_deref(), Some("/a-orig.md"));
    recorder.flush().await.unwrap();
    let rows = state.lock().unwrap().doc_access_rows.clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dentry_id, "d-a", "counts follow the displayed alias");
}

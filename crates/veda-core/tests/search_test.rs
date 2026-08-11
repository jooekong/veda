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
    // Mock mirrors real Milvus honoring the pushed-down id scope: only
    // the in-scope summary comes back.
    let summary_hits = vec![SearchHit {
        file_id: "f1".into(),
        dentry_id: None,
        chunk_index: None,
        content: "in docs".into(),
        score: 0.9,
        score_type: "cosine".into(),
        path: Some("/docs/a.md".into()),
        l0_abstract: Some("in docs".into()),
        l1_overview: None,
    }];
    // The prefix must exist as a real subtree now: scope resolution
    // short-circuits nonexistent paths to an empty result.
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dir_dentry("dir-docs", "ws1", "/docs"),
            dentry("d1", "ws1", "/docs/a.md", "f1"),
            dentry("d2", "ws1", "/src/b.rs", "f2"),
        ];
    }
    let vector = Arc::new(MockVector {
        chunk_hits: vec![],
        summary_hits,
    });
    let svc = SearchService::new(meta, vector, Arc::new(MockEmbedding));

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

// ── path_prefix scope pushdown ─────────────────────────────────────

/// VectorStore mock that records every request, so tests can assert on
/// what the service actually pushed down (id_filter, fetch limit,
/// normalized prefix) rather than just on the final hit set.
struct RecordingVector {
    chunk_hits: Vec<SearchHit>,
    summary_hits: Vec<SearchHit>,
    chunk_reqs: std::sync::Mutex<Vec<SearchRequest>>,
    summary_reqs: std::sync::Mutex<Vec<SearchRequest>>,
}

impl RecordingVector {
    fn new(chunk_hits: Vec<SearchHit>, summary_hits: Vec<SearchHit>) -> Self {
        Self {
            chunk_hits,
            summary_hits,
            chunk_reqs: std::sync::Mutex::new(vec![]),
            summary_reqs: std::sync::Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl VectorStore for RecordingVector {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
    async fn upsert_chunks(&self, _chunks: &[ChunkWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_chunks(&self, _ws: &str, _fid: &str) -> Result<()> {
        Ok(())
    }
    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        self.chunk_reqs.lock().unwrap().push(req.clone());
        Ok(self.chunk_hits.clone())
    }
    async fn upsert_summaries(&self, _summaries: &[SummaryWithEmbedding]) -> Result<()> {
        Ok(())
    }
    async fn delete_summary(&self, _ws: &str, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_summaries(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        self.summary_reqs.lock().unwrap().push(req.clone());
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

fn dir_dentry(id: &str, ws: &str, path: &str) -> Dentry {
    Dentry {
        id: id.into(),
        workspace_id: ws.into(),
        path: path.into(),
        parent_path: "/".into(),
        name: path.rsplit('/').next().unwrap_or("").into(),
        is_dir: true,
        file_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

/// The canonical /api-docs fixture: a small subtree next to a large
/// sibling, the exact shape that starved the post-filter in production.
fn scoped_service(vector: Arc<RecordingVector>) -> SearchService {
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dir_dentry("dir-api", "ws1", "/api-docs"),
            dir_dentry("dir-sub", "ws1", "/api-docs/sub"),
            dentry("da", "ws1", "/api-docs/a.json", "fa"),
            dentry("db", "ws1", "/api-docs/sub/b.json", "fb"),
            dentry("dz", "ws1", "/api-docs-v2/x.md", "fz"),
            dentry("dbiz", "ws1", "/biz/c.md", "fc"),
        ];
    }
    SearchService::new(meta, vector, Arc::new(MockEmbedding))
}

#[tokio::test]
async fn prefix_pushes_subtree_file_ids_down() {
    // Mock mirrors real Milvus: with id_filter honored, only in-scope
    // hits come back.
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let svc = scoped_service(vector.clone());
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/api-docs"),
            DetailLevel::Full,
        )
        .await
        .unwrap();

    let reqs = vector.chunk_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let mut ids = reqs[0].id_filter.clone().expect("scope must be pushed down");
    ids.sort();
    assert_eq!(ids, vec!["fa".to_string(), "fb".to_string()]);
    // Scoped retrieval ranks inside the subtree: fetch exactly `limit`,
    // not the 3x over-fetch the global fallback needs.
    assert_eq!(reqs[0].limit, 10);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].path.as_deref(), Some("/api-docs/a.json"));
}

#[tokio::test]
async fn pushdown_keeps_hits_whose_alias_path_sits_outside_the_prefix() {
    // COW copy-alias: fa lives at /api-docs/a.json AND /aaa/x.json, and
    // batch path resolution deterministically picks the smallest path
    // (/aaa/x.json). The file IS in scope — the id filter proved it —
    // so the hit must survive even though its displayed path is outside
    // the prefix. The old byte-level retain silently dropped this.
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dir_dentry("dir-api", "ws1", "/api-docs"),
            dentry("da", "ws1", "/api-docs/a.json", "fa"),
            dentry("dalias", "ws1", "/aaa/x.json", "fa"),
        ];
    }
    let svc = SearchService::new(meta, vector, Arc::new(MockEmbedding));
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/api-docs"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    assert_eq!(out.len(), 1, "in-scope file must not be dropped over its alias path");
    assert_eq!(out[0].path.as_deref(), Some("/aaa/x.json"));
}

#[tokio::test]
async fn file_path_as_prefix_scopes_to_that_single_file() {
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let svc = scoped_service(vector.clone());
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/api-docs/a.json"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    let reqs = vector.chunk_reqs.lock().unwrap();
    assert_eq!(
        reqs[0].id_filter.as_deref(),
        Some(&["fa".to_string()][..]),
        "a file path as prefix means: search this one file"
    );
    assert_eq!(out.len(), 1);
}

#[tokio::test]
async fn prefix_is_normalized_leniently() {
    // "api-docs/" (no lead slash, trailing slash) must scope identically
    // to "/api-docs" — CLI and workbench callers pass both shapes.
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let svc = scoped_service(vector.clone());
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("api-docs/"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    let reqs = vector.chunk_reqs.lock().unwrap();
    assert_eq!(reqs[0].path_prefix.as_deref(), Some("/api-docs"));
    assert!(reqs[0].id_filter.is_some());
    assert_eq!(out.len(), 1);
}

#[tokio::test]
async fn prefix_scope_excludes_path_boundary_siblings() {
    // /api-docs must not swallow /api-docs-v2: the sibling's file must
    // not enter the pushed-down scope (LIKE 'prefix/%' boundary).
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let svc = scoped_service(vector.clone());
    svc.search(
        "ws1",
        "q",
        SearchMode::Hybrid,
        10,
        Some("/api-docs"),
        DetailLevel::Full,
    )
    .await
    .unwrap();
    let reqs = vector.chunk_reqs.lock().unwrap();
    assert!(
        !reqs[0].id_filter.as_ref().unwrap().contains(&"fz".to_string()),
        "path-boundary sibling leaked into the scope"
    );
}

#[tokio::test]
async fn nonexistent_prefix_short_circuits_to_empty() {
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let svc = scoped_service(vector.clone());
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/no-such-dir"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    assert!(out.is_empty());
    // Empty scope must never reach the vector store (`in []` is invalid).
    assert!(vector.chunk_reqs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_subtree_falls_back_to_global_post_filter() {
    // Global retrieval can return anything, including a path-boundary
    // sibling (/big-v2) — the fallback post-filter must drop it.
    let vector = Arc::new(RecordingVector::new(
        vec![chunk_hit("fa", 0), chunk_hit("fsib", 0)],
        vec![],
    ));
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries.push(dir_dentry("dir-big", "ws1", "/big"));
        st.dentries.push(dentry("da", "ws1", "/big/keep.md", "fa"));
        st.dentries
            .push(dentry("dsib", "ws1", "/big-v2/x.md", "fsib"));
        for i in 0..1001 {
            let fid = format!("f{i}");
            st.dentries
                .push(dentry(&format!("d{i}"), "ws1", &format!("/big/n{i:04}.md"), &fid));
        }
    }
    let svc = SearchService::new(meta, vector.clone(), Arc::new(MockEmbedding));
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/big"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    let reqs = vector.chunk_reqs.lock().unwrap();
    assert!(
        reqs[0].id_filter.is_none(),
        "over-cap subtree must fall back to unscoped retrieval"
    );
    assert_eq!(reqs[0].limit, 30, "fallback keeps the 3x over-fetch");
    assert_eq!(out.len(), 1, "post-filter still applies in fallback");
    assert_eq!(
        out[0].path.as_deref(),
        Some("/big/keep.md"),
        "boundary sibling /big-v2 must not survive the fallback filter"
    );
}

#[tokio::test]
async fn abstract_scope_carries_dir_ids_and_resolves_dir_hits() {
    // Directory summaries live under the *dentry* id in the summary
    // collection. The scope must include them, and a directory hit must
    // resolve to a path (it used to stay path-less and get dropped by
    // the prefix filter even at rank 1).
    let dir_hit = SearchHit {
        file_id: "dir-sub".into(), // summary id slot: dentry id of /api-docs/sub
        dentry_id: None,
        chunk_index: None,
        content: "sub dir summary".into(),
        score: 0.9,
        score_type: "cosine".into(),
        path: None,
        l0_abstract: Some("sub dir summary".into()),
        l1_overview: None,
    };
    let vector = Arc::new(RecordingVector::new(vec![], vec![dir_hit]));
    let svc = scoped_service(vector.clone());
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Semantic,
            10,
            Some("/api-docs"),
            DetailLevel::Abstract,
        )
        .await
        .unwrap();

    let reqs = vector.summary_reqs.lock().unwrap();
    let mut ids = reqs[0].id_filter.clone().expect("summary scope pushed down");
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "dir-api".to_string(),
            "dir-sub".to_string(),
            "fa".to_string(),
            "fb".to_string()
        ],
        "scope carries file ids AND directory dentry ids"
    );

    assert_eq!(out.len(), 1, "directory-summary hit must survive the prefix filter");
    assert_eq!(out[0].path.as_deref(), Some("/api-docs/sub"));
    assert!(
        out[0].dentry_id.is_none(),
        "directory hits stay out of access stats"
    );
}

#[tokio::test]
async fn full_level_dir_only_scope_returns_empty_without_vector_call() {
    // A prefix that contains only directories can't produce chunk hits;
    // the service must short-circuit instead of shipping `in []`.
    let vector = Arc::new(RecordingVector::new(vec![chunk_hit("fa", 0)], vec![]));
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries.push(dir_dentry("dir-empty", "ws1", "/empty"));
        st.dentries.push(dir_dentry("dir-inner", "ws1", "/empty/inner"));
    }
    let svc = SearchService::new(meta, vector.clone(), Arc::new(MockEmbedding));
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Hybrid,
            10,
            Some("/empty"),
            DetailLevel::Full,
        )
        .await
        .unwrap();
    assert!(out.is_empty());
    assert!(vector.chunk_reqs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn get_summary_tolerates_trailing_slash() {
    // `veda abstract /docs/dal/` used to 404: dentry paths are stored
    // canonical and the lookup didn't normalize.
    let vector = Arc::new(RecordingVector::new(vec![], vec![]));
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries.push(dir_dentry("dir-api", "ws1", "/api-docs"));
        st.dir_summaries.insert(
            "dir-api".into(),
            FileSummary {
                id: "s1".into(),
                workspace_id: "ws1".into(),
                file_id: None,
                dentry_id: Some("dir-api".into()),
                l0_abstract: "api docs".into(),
                l1_overview: "api docs overview".into(),
                status: SummaryStatus::Ready,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );
    }
    let svc = SearchService::new(meta, vector, Arc::new(MockEmbedding));
    let s = svc
        .get_summary("ws1", "/api-docs/")
        .await
        .unwrap()
        .expect("trailing slash must resolve to the canonical dentry");
    assert_eq!(s.l0_abstract, "api docs");
}

#[tokio::test]
async fn overview_backfills_l1_for_directory_hits() {
    // Overview on a directory-summary hit: L1 must come from the
    // dentry-keyed summary table, not stay None (codex review P2).
    let dir_hit = SearchHit {
        file_id: "dir-sub".into(),
        dentry_id: None,
        chunk_index: None,
        content: "sub dir l0".into(),
        score: 0.9,
        score_type: "cosine".into(),
        path: None,
        l0_abstract: Some("sub dir l0".into()),
        l1_overview: None,
    };
    let vector = Arc::new(RecordingVector::new(vec![], vec![dir_hit]));
    let meta = Arc::new(mock_store::MockMetadataStore::new());
    {
        let mut st = meta.state.lock().unwrap();
        st.dentries = vec![
            dir_dentry("dir-api", "ws1", "/api-docs"),
            dir_dentry("dir-sub", "ws1", "/api-docs/sub"),
        ];
        st.dir_summaries.insert(
            "dir-sub".into(),
            FileSummary {
                id: "s-sub".into(),
                workspace_id: "ws1".into(),
                file_id: None,
                dentry_id: Some("dir-sub".into()),
                l0_abstract: "sub dir l0".into(),
                l1_overview: "sub dir L1 overview".into(),
                status: SummaryStatus::Ready,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );
    }
    let svc = SearchService::new(meta, vector, Arc::new(MockEmbedding));
    let out = svc
        .search(
            "ws1",
            "q",
            SearchMode::Semantic,
            10,
            Some("/api-docs"),
            DetailLevel::Overview,
        )
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].path.as_deref(), Some("/api-docs/sub"));
    assert_eq!(
        out[0].l1_overview.as_deref(),
        Some("sub dir L1 overview"),
        "directory hit must carry its L1 at overview level"
    );
}

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
        chunk_index: Some(0),
        content: "chunk content".into(),
        score: 0.9,
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
        chunk_index: None,
        content: "L0 abstract text".into(),
        score: 0.95,
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
            chunk_index: None,
            content: "in docs".into(),
            score: 0.9,
            path: Some("/docs/a.md".into()),
            l0_abstract: Some("in docs".into()),
            l1_overview: None,
        },
        SearchHit {
            file_id: "f2".into(),
            chunk_index: None,
            content: "in src".into(),
            score: 0.8,
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

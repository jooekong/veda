use std::sync::Arc;

use tracing::warn;
use veda_types::*;

use crate::store::{EmbeddingService, MetadataStore, VectorStore};

pub struct SearchService {
    meta: Arc<dyn MetadataStore>,
    vector: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingService>,
}

impl SearchService {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        vector: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            meta,
            vector,
            embedding,
        }
    }

    pub async fn search(
        &self,
        workspace_id: &str,
        query: &str,
        mode: SearchMode,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let limit = if limit == 0 { 10 } else { limit };
        let fetch_limit = if path_prefix.is_some() { limit * 3 } else { limit };

        let mut hits = match mode {
            SearchMode::Semantic => {
                let vectors = self.embedding.embed(&[query.to_string()]).await?;
                let query_vector = vectors.into_iter().next().ok_or_else(|| {
                    VedaError::EmbeddingFailed("empty embedding result".to_string())
                })?;
                let req = SearchRequest {
                    workspace_id: workspace_id.to_string(),
                    query: query.to_string(),
                    mode: SearchMode::Semantic,
                    limit: fetch_limit,
                    path_prefix: path_prefix.map(|s| s.to_string()),
                    query_vector: Some(query_vector),
                };
                self.vector.search(&req).await?
            }
            SearchMode::Fulltext => {
                let req = SearchRequest {
                    workspace_id: workspace_id.to_string(),
                    query: query.to_string(),
                    mode: SearchMode::Fulltext,
                    limit: fetch_limit,
                    path_prefix: path_prefix.map(|s| s.to_string()),
                    query_vector: None,
                };
                self.vector.search(&req).await?
            }
            SearchMode::Hybrid => {
                let vectors = self.embedding.embed(&[query.to_string()]).await?;
                let query_vector = vectors.into_iter().next().ok_or_else(|| {
                    VedaError::EmbeddingFailed("empty embedding result".to_string())
                })?;
                let req = HybridSearchRequest {
                    workspace_id: workspace_id.to_string(),
                    query_vector,
                    query_text: Some(query.to_string()),
                    mode: SearchMode::Hybrid,
                    limit: fetch_limit,
                };
                self.vector.hybrid_search(&req).await?
            }
        };

        for hit in &mut hits {
            if hit.path.is_none() {
                match self
                    .meta
                    .get_dentry_path_by_file_id(workspace_id, &hit.file_id)
                    .await
                {
                    Ok(p) => hit.path = p,
                    Err(e) => {
                        warn!(file_id = %hit.file_id, err = %e, "failed to resolve path for search hit");
                    }
                }
            }
        }

        if let Some(prefix) = path_prefix {
            hits.retain(|h| {
                h.path.as_ref().map_or(false, |p| p.starts_with(prefix))
            });
        }

        hits.truncate(limit);
        Ok(hits)
    }
}

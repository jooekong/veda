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
        detail_level: DetailLevel,
    ) -> Result<Vec<SearchHit>> {
        match detail_level {
            DetailLevel::Abstract => self.search_abstract(workspace_id, query, mode, limit, path_prefix).await,
            DetailLevel::Overview => self.search_overview(workspace_id, query, mode, limit, path_prefix).await,
            DetailLevel::Full => self.search_full(workspace_id, query, mode, limit, path_prefix).await,
        }
    }

    async fn search_abstract(
        &self,
        workspace_id: &str,
        query: &str,
        mode: SearchMode,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        if mode != SearchMode::Semantic {
            warn!(requested_mode = ?mode, "abstract/overview search always uses semantic mode, ignoring requested mode");
        }
        let limit = if limit == 0 { 10 } else { limit };
        let fetch_limit = if path_prefix.is_some() { limit * 3 } else { limit };

        // Summary search is always vector-based (L0 abstracts are short
        // semantic texts), so we always embed regardless of the requested mode.
        let vectors = self.embedding.embed(&[query.to_string()]).await?;
        let query_vector = Some(vectors.into_iter().next().ok_or_else(|| {
            VedaError::EmbeddingFailed("empty embedding result".to_string())
        })?);

        let req = SearchRequest {
            workspace_id: workspace_id.to_string(),
            query: query.to_string(),
            mode: SearchMode::Semantic,
            limit: fetch_limit,
            path_prefix: path_prefix.map(|s| s.to_string()),
            query_vector,
        };

        let mut hits = self.vector.search_summaries(&req).await?;
        self.resolve_paths(workspace_id, &mut hits).await;

        if let Some(prefix) = path_prefix {
            hits.retain(|h| h.path.as_ref().map_or(false, |p| p.starts_with(prefix)));
        }
        hits.truncate(limit);
        Ok(hits)
    }

    async fn search_overview(
        &self,
        workspace_id: &str,
        query: &str,
        mode: SearchMode,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let mut hits = self
            .search_abstract(workspace_id, query, mode, limit, path_prefix)
            .await?;

        let file_ids: Vec<String> = hits.iter().map(|h| h.file_id.clone()).collect();
        if !file_ids.is_empty() {
            let summaries = self.meta.get_summaries_by_file_ids(&file_ids).await?;
            for hit in &mut hits {
                if let Some(summary) = summaries.get(&hit.file_id) {
                    hit.l1_overview = Some(summary.l1_overview.clone());
                }
            }
        }
        Ok(hits)
    }

    async fn search_full(
        &self,
        workspace_id: &str,
        query: &str,
        mode: SearchMode,
        limit: usize,
        path_prefix: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let limit = if limit == 0 { 10 } else { limit };
        let fetch_limit = if path_prefix.is_some() {
            limit * 3
        } else {
            limit
        };

        let query_vector = match mode {
            SearchMode::Semantic | SearchMode::Hybrid => {
                let vectors = self.embedding.embed(&[query.to_string()]).await?;
                Some(vectors.into_iter().next().ok_or_else(|| {
                    VedaError::EmbeddingFailed("empty embedding result".to_string())
                })?)
            }
            SearchMode::Fulltext => None,
        };

        let req = SearchRequest {
            workspace_id: workspace_id.to_string(),
            query: query.to_string(),
            mode,
            limit: fetch_limit,
            path_prefix: path_prefix.map(|s| s.to_string()),
            query_vector,
        };
        let mut hits = self.vector.search(&req).await?;
        self.resolve_paths(workspace_id, &mut hits).await;

        if let Some(prefix) = path_prefix {
            hits.retain(|h| h.path.as_ref().map_or(false, |p| p.starts_with(prefix)));
        }

        hits.truncate(limit);
        Ok(hits)
    }

    async fn resolve_paths(&self, workspace_id: &str, hits: &mut [SearchHit]) {
        let missing_fids: Vec<String> = hits
            .iter()
            .filter(|h| h.path.is_none())
            .map(|h| h.file_id.clone())
            .collect();
        if missing_fids.is_empty() {
            return;
        }
        match self
            .meta
            .get_dentry_paths_by_file_ids(workspace_id, &missing_fids)
            .await
        {
            Ok(path_map) => {
                for hit in hits.iter_mut() {
                    if hit.path.is_none() {
                        hit.path = path_map.get(&hit.file_id).cloned();
                    }
                }
            }
            Err(e) => {
                warn!(err = %e, "failed to batch-resolve paths for search hits");
            }
        }
    }

    pub async fn get_summary(
        &self,
        workspace_id: &str,
        path: &str,
    ) -> Result<Option<FileSummary>> {
        let dentry = self.meta.get_dentry(workspace_id, path).await?;
        let Some(dentry) = dentry else {
            return Err(VedaError::NotFound(format!("path not found: {path}")));
        };

        if dentry.is_dir {
            self.meta.get_summary_by_dentry(&dentry.id).await
        } else if let Some(file_id) = &dentry.file_id {
            self.meta.get_summary_by_file(file_id).await
        } else {
            Ok(None)
        }
    }
}

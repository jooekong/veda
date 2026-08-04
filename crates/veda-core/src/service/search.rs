use std::sync::Arc;

use tracing::warn;
use unicode_normalization::UnicodeNormalization;
use veda_types::*;

use crate::store::{EmbeddingService, MetadataStore, VectorStore};

/// Approximate MySQL's `utf8mb4_0900_ai_ci` folding for one path segment:
/// case-insensitive and accent-insensitive.
///
/// Needed because path comparison in veda happens in the database — the
/// `path` column carries that collation and `get_dentry` / `list_dentries`
/// compare against it directly — while the workspace layout has to line
/// database-side grouping results up with dentry names in Rust. Decompose to
/// NFD and drop combining marks, which is what "accent-insensitive" means
/// for the Latin range; then lowercase.
///
/// This is deliberately an approximation of a full collation table. A
/// segment it folds differently from MySQL loses its `file_count` (reported
/// as 0). Not airtight the other way either: this strips ALL combining
/// marks while MySQL keeps primary-weight ones (Thai/Indic vowel signs)
/// distinct, so two MySQL-distinct directories can collide on one folded
/// key and one shows the other's count. Accepted as-is: the CJK/Latin
/// names this deployment actually has cannot hit that case.
fn fold_path_segment(segment: &str) -> String {
    segment
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Cloneable: every field is an `Arc`, so cloning is cheap ref-count bumps.
/// `AnswerService` holds its own clone rather than an `Arc<SearchService>`.
#[derive(Clone)]
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
            DetailLevel::Abstract => {
                self.search_abstract(workspace_id, query, mode, limit, path_prefix)
                    .await
            }
            DetailLevel::Overview => {
                self.search_overview(workspace_id, query, mode, limit, path_prefix)
                    .await
            }
            DetailLevel::Full => {
                self.search_full(workspace_id, query, mode, limit, path_prefix)
                    .await
            }
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
        let fetch_limit = if path_prefix.is_some() {
            limit * 3
        } else {
            limit
        };

        // Summary search is always vector-based (L0 abstracts are short
        // semantic texts), so we always embed regardless of the requested mode.
        let vectors = self.embedding.embed(&[query.to_string()]).await?;
        let query_vector = Some(
            vectors
                .into_iter()
                .next()
                .ok_or_else(|| VedaError::EmbeddingFailed("empty embedding result".to_string()))?,
        );

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

    pub async fn get_summary(&self, workspace_id: &str, path: &str) -> Result<Option<FileSummary>> {
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

    /// Assemble the workspace layout: top-level entries plus a one-line summary
    /// per area. Pure assembly of data that already exists — no LLM call.
    ///
    /// This *is* the root-level view. The workspace root has no dentry, so
    /// it has no L0/L1 row to serve (a bare `/v1/abstract` route was tried
    /// and removed for producing misleading 404s); building the view from
    /// the children sidesteps that instead of teaching the worker to
    /// summarise the root.
    ///
    /// Returns `Ready` or `Partial`. Only the caller knows whether summary
    /// generation is configured at all, so promoting to `Disabled` is the
    /// HTTP layer's job.
    pub async fn workspace_layout(&self, workspace_id: &str, cap: usize) -> Result<api::WorkspaceLayout> {
        // Over-fetch by one so "is there more?" costs no extra query.
        let mut children = self
            .meta
            .list_children_capped(workspace_id, "/", cap + 1)
            .await?;
        let truncated = children.len() > cap;
        children.truncate(cap);

        let file_ids: Vec<String> = children.iter().filter_map(|d| d.file_id.clone()).collect();
        let dir_ids: Vec<String> = children
            .iter()
            .filter(|d| d.is_dir)
            .map(|d| d.id.clone())
            .collect();

        let files = self.meta.get_files_batch(&file_ids).await?;
        let sizes: std::collections::HashMap<&str, i64> =
            files.iter().map(|f| (f.id.as_str(), f.size_bytes)).collect();
        let file_summaries = self.meta.get_summaries_by_file_ids(&file_ids).await?;
        let dir_summaries = self.meta.get_summaries_by_dentry_ids(&dir_ids).await?;
        // MySQL groups these segments under the path column's collation and
        // returns one arbitrary spelling per group, so a directory named
        // `Docs` may come back keyed `docs` and `café` keyed `cafe`. Fold
        // both sides the same way or the lookup below silently reports 0.
        let counts: std::collections::HashMap<String, i64> = self
            .meta
            .count_files_by_top_level(workspace_id)
            .await?
            .into_iter()
            .map(|(k, v)| (fold_path_segment(&k), v))
            .collect();
        let stats = self.meta.storage_stats(workspace_id).await?;

        let entries: Vec<api::LayoutEntry> = children
            .into_iter()
            .map(|d| {
                let summary = if d.is_dir {
                    dir_summaries.get(&d.id)
                } else {
                    d.file_id.as_deref().and_then(|fid| file_summaries.get(fid))
                };
                api::LayoutEntry {
                    // Key the count off is_dir, never off "the counts map happens to
                    // have this name" — a root-level file groups under its own
                    // file name and would otherwise report a bogus count.
                    file_count: if d.is_dir {
                        Some(counts.get(&fold_path_segment(&d.name)).copied().unwrap_or(0))
                    } else {
                        None
                    },
                    size_bytes: d
                        .file_id
                        .as_deref()
                        .and_then(|fid| sizes.get(fid).copied()),
                    l0_abstract: summary.map(|s| s.l0_abstract.clone()),
                    path: d.path,
                    is_dir: d.is_dir,
                }
            })
            .collect();

        // Coverage is over what we return, not the whole workspace: entries
        // dropped by truncation must not drag the state down to Partial.
        let summary_state = if entries.iter().all(|e| e.l0_abstract.is_some()) {
            api::LayoutSummaryState::Ready
        } else {
            api::LayoutSummaryState::Partial
        };

        Ok(api::WorkspaceLayout {
            stats,
            summary_state,
            truncated,
            entries,
        })
    }
}

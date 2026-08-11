use std::sync::Arc;

use tracing::warn;
use veda_types::*;

use crate::path::fold_path_segment;
use crate::service::access_stats::AccessRecorder;
use crate::store::{EmbeddingService, MetadataStore, VectorStore};

/// Above this many dentries under a path_prefix, scope pushdown falls
/// back to global-search-then-filter: the id list would bloat the Milvus
/// filter expression (~40 bytes per id). At 1000 the expression stays
/// ~40KB, well inside request-body limits, and directories that large
/// hold enough of the workspace that the global candidate window isn't
/// starved the way a 4%-of-corpus directory is.
const SCOPE_CAP: usize = 1000;

/// Subtree ids resolved from a path_prefix, split by what each search
/// layer filters on: chunks live under file ids, directory summaries
/// under dentry ids.
struct SearchScope {
    file_ids: Vec<String>,
    dir_ids: Vec<String>,
}

impl SearchScope {
    fn is_empty(&self) -> bool {
        self.file_ids.is_empty() && self.dir_ids.is_empty()
    }
    /// Filter for the summary collection, whose `id` column holds
    /// file_ids for file summaries and dentry_ids for directory ones.
    fn summary_ids(&self) -> Vec<String> {
        let mut ids = self.file_ids.clone();
        ids.extend(self.dir_ids.iter().cloned());
        ids
    }
}

/// Cloneable: every field is an `Arc`, so cloning is cheap ref-count bumps.
/// `AnswerService` holds its own clone rather than an `Arc<SearchService>`.
#[derive(Clone)]
pub struct SearchService {
    meta: Arc<dyn MetadataStore>,
    vector: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    stats: Arc<AccessRecorder>,
}

impl SearchService {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        vector: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingService>,
    ) -> Self {
        let stats = Arc::new(AccessRecorder::disabled(meta.clone()));
        Self {
            meta,
            vector,
            embedding,
            stats,
        }
    }

    /// Production constructor: searches through this service bump
    /// per-document hit counters on `recorder`.
    pub fn with_stats(
        meta: Arc<dyn MetadataStore>,
        vector: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingService>,
        recorder: Arc<AccessRecorder>,
    ) -> Self {
        Self {
            meta,
            vector,
            embedding,
            stats: recorder,
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
        // Lenient prefix normalization at the single choke point every
        // surface (REST, MCP, platform gateway, SQL UDTF) flows through:
        // "api-docs/", "/api-docs/" and "/api-docs" all mean the same
        // subtree, and "/" means no restriction at all.
        let normalized_prefix = match path_prefix {
            Some(raw) => {
                let p = crate::path::normalize_lenient(raw)?;
                if p == "/" {
                    None
                } else {
                    Some(p)
                }
            }
            None => None,
        };
        let path_prefix = normalized_prefix.as_deref();

        // Resolve the subtree id scope so retrieval ranks INSIDE the
        // prefix instead of hoping the subtree surfaces in a global
        // top-K (it doesn't, for small directories — see repro of
        // 2026-08-11: /api-docs at 4% of corpus scored 0/30 candidates
        // on generic queries). `None` scope = no prefix or subtree too
        // large; empty scope = prefix exists but holds nothing.
        let scope = match path_prefix {
            Some(p) => self.resolve_scope(workspace_id, p).await?,
            None => None,
        };
        if let Some(s) = &scope {
            if s.is_empty() {
                return Ok(vec![]);
            }
        }
        let scope = scope.as_ref();

        let hits = match detail_level {
            DetailLevel::Abstract => {
                self.search_abstract(workspace_id, query, mode, limit, path_prefix, scope)
                    .await?
            }
            DetailLevel::Overview => {
                self.search_overview(workspace_id, query, mode, limit, path_prefix, scope)
                    .await?
            }
            DetailLevel::Full => {
                self.search_full(workspace_id, query, mode, limit, path_prefix, scope)
                    .await?
            }
        };
        // Heat counting on the FINAL hit set — after prefix filtering and
        // truncation, so only what the caller actually receives counts.
        // Dedup per query (3 chunks of one file = 1 impression). Hits that
        // never resolved (detached file_ids, directory-summary hits whose
        // `file_id` is really a dentry_id) have no `dentry_id` and are
        // skipped by construction.
        let mut seen = std::collections::HashSet::new();
        let dentry_ids: Vec<String> = hits
            .iter()
            .filter_map(|h| h.dentry_id.clone())
            .filter(|id| seen.insert(id.clone()))
            .collect();
        self.stats.record_search_hits(workspace_id, &dentry_ids);
        Ok(hits)
    }

    /// Enumerate the subtree under `prefix` (plus the prefix entry
    /// itself) into an id scope. Returns `None` when the subtree
    /// exceeds [`SCOPE_CAP`] — the caller then falls back to
    /// global-search-then-filter rather than shipping an unbounded id
    /// list to the vector store.
    async fn resolve_scope(
        &self,
        workspace_id: &str,
        prefix: &str,
    ) -> Result<Option<SearchScope>> {
        let mut scope = SearchScope {
            file_ids: Vec::new(),
            dir_ids: Vec::new(),
        };
        // The prefix entry itself is part of the scope: a directory's
        // own summary is a legitimate hit for a search scoped to it,
        // and a *file* path as prefix means "search this one file"
        // (the LIKE 'prefix/%' listing below can't see either).
        match self.meta.get_dentry(workspace_id, prefix).await? {
            Some(d) if d.is_dir => scope.dir_ids.push(d.id),
            Some(d) => {
                if let Some(fid) = d.file_id {
                    scope.file_ids.push(fid);
                }
            }
            None => return Ok(Some(scope)), // nonexistent path: empty scope, empty result
        }
        let children = self
            .meta
            .list_dentries_under_page(workspace_id, prefix, None, SCOPE_CAP + 1)
            .await?;
        if children.len() > SCOPE_CAP {
            warn!(
                prefix,
                cap = SCOPE_CAP,
                "path_prefix subtree exceeds scope cap; falling back to global search + post-filter (coverage may be incomplete)"
            );
            return Ok(None);
        }
        for d in children {
            if d.is_dir {
                scope.dir_ids.push(d.id);
            } else if let Some(fid) = d.file_id {
                scope.file_ids.push(fid);
            }
        }
        Ok(Some(scope))
    }

    /// Prefix match with a path-boundary: `/api-docs` must not swallow
    /// `/api-docs-v2/…`. FALLBACK-ONLY post-filter (subtree over the
    /// scope cap → global retrieval). When the id scope was pushed
    /// down, every hit is in the subtree by construction and a
    /// byte-level path comparison could only wrongly drop hits: the DB
    /// compares paths with `utf8mb4_0900_ai_ci` (a stored `/Docs`
    /// subtree resolves for a requested `/docs` prefix), and a COW
    /// copy-alias resolves to its smallest path which may sit outside
    /// the subtree even though the file itself is in scope.
    fn prefix_matches(path: &str, prefix: &str) -> bool {
        path == prefix
            || (path.len() > prefix.len()
                && path.starts_with(prefix)
                && path.as_bytes()[prefix.len()] == b'/')
    }

    async fn search_abstract(
        &self,
        workspace_id: &str,
        query: &str,
        mode: SearchMode,
        limit: usize,
        path_prefix: Option<&str>,
        scope: Option<&SearchScope>,
    ) -> Result<Vec<SearchHit>> {
        if mode != SearchMode::Semantic {
            warn!(requested_mode = ?mode, "abstract/overview search always uses semantic mode, ignoring requested mode");
        }
        let limit = if limit == 0 { 10 } else { limit };
        // With the scope pushed down, retrieval already ranks inside the
        // subtree — fetch exactly `limit`. Only the fallback (prefix set
        // but subtree over cap) still over-fetches for its post-filter.
        let fetch_limit = if path_prefix.is_some() && scope.is_none() {
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
            id_filter: scope.map(|s| s.summary_ids()),
        };

        let mut hits = self.vector.search_summaries(&req).await?;
        self.resolve_paths(workspace_id, &mut hits).await;

        if scope.is_none() {
            if let Some(prefix) = path_prefix {
                hits.retain(|h| {
                    h.path
                        .as_ref()
                        .map_or(false, |p| Self::prefix_matches(p, prefix))
                });
            }
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
        scope: Option<&SearchScope>,
    ) -> Result<Vec<SearchHit>> {
        let mut hits = self
            .search_abstract(workspace_id, query, mode, limit, path_prefix, scope)
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
        // Directory-summary hits carry the dentry id in the id slot, so
        // the file-keyed lookup above can't fill their L1 — fetch those
        // by dentry id or directory hits ship as L0-only Overview rows.
        let dir_ids: Vec<String> = hits
            .iter()
            .filter(|h| h.l1_overview.is_none())
            .map(|h| h.file_id.clone())
            .collect();
        if !dir_ids.is_empty() {
            let dir_summaries = self.meta.get_summaries_by_dentry_ids(&dir_ids).await?;
            for hit in &mut hits {
                if hit.l1_overview.is_none() {
                    if let Some(summary) = dir_summaries.get(&hit.file_id) {
                        hit.l1_overview = Some(summary.l1_overview.clone());
                    }
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
        scope: Option<&SearchScope>,
    ) -> Result<Vec<SearchHit>> {
        let limit = if limit == 0 { 10 } else { limit };
        let fetch_limit = if path_prefix.is_some() && scope.is_none() {
            limit * 3
        } else {
            limit
        };
        // Chunks only exist for files. A scope with directories but no
        // files can't produce full-level hits, so skip the round-trip
        // (also keeps the `Some(vec![])` contract off the stores).
        if let Some(s) = scope {
            if s.file_ids.is_empty() {
                return Ok(vec![]);
            }
        }

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
            id_filter: scope.map(|s| s.file_ids.clone()),
        };
        let mut hits = self.vector.search(&req).await?;
        self.resolve_paths(workspace_id, &mut hits).await;

        if scope.is_none() {
            if let Some(prefix) = path_prefix {
                hits.retain(|h| {
                    h.path
                        .as_ref()
                        .map_or(false, |p| Self::prefix_matches(p, prefix))
                });
            }
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
                        if let Some(r) = path_map.get(&hit.file_id) {
                            hit.path = Some(r.path.clone());
                            hit.dentry_id = Some(r.dentry_id.clone());
                        }
                    }
                }
            }
            Err(e) => {
                warn!(err = %e, "failed to batch-resolve paths for search hits");
            }
        }

        // Second pass: ids that didn't resolve as file_ids are directory
        // dentry ids (directory-summary hits store the dentry id in the
        // id slot — a directory has no file). Without this, every
        // directory summary surfaced path-less and a path_prefix filter
        // silently dropped it, even at rank 1. `dentry_id` stays None on
        // purpose: access stats count *document* reads/impressions and a
        // directory hit is not a document.
        let unresolved: Vec<String> = hits
            .iter()
            .filter(|h| h.path.is_none())
            .map(|h| h.file_id.clone())
            .collect();
        if unresolved.is_empty() {
            return;
        }
        match self
            .meta
            .get_dentry_paths_by_ids(workspace_id, &unresolved)
            .await
        {
            Ok(dir_map) => {
                for hit in hits.iter_mut() {
                    if hit.path.is_none() {
                        if let Some(p) = dir_map.get(&hit.file_id) {
                            hit.path = Some(p.clone());
                        }
                    }
                }
            }
            Err(e) => {
                warn!(err = %e, "failed to batch-resolve directory paths for summary hits");
            }
        }
    }

    pub async fn get_summary(&self, workspace_id: &str, path: &str) -> Result<Option<FileSummary>> {
        // Dentry paths are stored canonical; a raw `/docs/dal/` from the
        // CLI or MCP would 404 on the trailing slash without this.
        let path = crate::path::normalize_lenient(path)?;
        let path = path.as_str();
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use veda_core::store::*;
use veda_types::*;

// ── In-memory MetadataStore ────────────────────────────

#[derive(Default, Clone)]
pub struct MockState {
    pub dentries: Vec<Dentry>,
    pub files: Vec<FileRecord>,
    pub file_contents: HashMap<String, String>,
    pub file_blobs: HashMap<String, Vec<u8>>,
    pub file_extracts: HashMap<String, FileExtract>,
    pub file_chunks: Vec<FileChunk>,
    pub outbox: Vec<OutboxEvent>,
    pub fs_events: Vec<FsEvent>,
    /// Summaries keyed by file_id, for `get_summaries_by_file_ids`.
    pub file_summaries: HashMap<String, FileSummary>,
    /// Summaries keyed by dentry_id, for `get_summaries_by_dentry_ids`.
    pub dir_summaries: HashMap<String, FileSummary>,
    /// Pre-computed answer for `count_files_by_top_level`.
    pub top_level_counts: HashMap<String, i64>,
    /// Every `limit` `list_children_capped` was called with. The workspace
    /// map must push its cap down into the query rather than reading
    /// everything and truncating afterwards; asserting on the returned
    /// length alone cannot tell those two implementations apart.
    pub children_capped_limits: Vec<usize>,
    /// How many ids each batch lookup was handed. The map must build its
    /// `IN (...)` lists from the *truncated* entry set, so these have to
    /// stay bounded by the cap however large the workspace is.
    pub batch_id_counts: Vec<(&'static str, usize)>,
    /// Rows captured by `upsert_doc_access_daily` (access-stats tests).
    pub doc_access_rows: Vec<DocAccessRow>,
    /// Arm `upsert_doc_access_daily` to fail (flush drop-window tests).
    pub fail_doc_access_upsert: bool,
}

pub struct MockMetadataStore {
    pub state: Arc<Mutex<MockState>>,
}

impl MockMetadataStore {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }
}

/// Mirrors the MySQL impl: returns chunks overlapping the line range [start, end].
/// The "containing" chunk (whose own start_line may be before `start`) is included.
/// Mirrors the post-W4.2 fix: chunks whose line range ends before `start_line`
/// are excluded, so a query past EOF returns empty rather than the last chunk.
fn filter_overlap(
    mut chunks: Vec<FileChunk>,
    start_line: Option<i32>,
    end_line: Option<i32>,
) -> Vec<FileChunk> {
    if let Some(b) = end_line {
        chunks.retain(|c| c.start_line <= b);
    }
    if let Some(a) = start_line {
        let base_idx = chunks
            .iter()
            .filter(|c| c.start_line <= a)
            .map(|c| c.chunk_index)
            .max()
            .unwrap_or(0);
        chunks.retain(|c| c.chunk_index >= base_idx && c.start_line + c.line_count >= a);
    }
    chunks
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .find(|d| d.workspace_id == workspace_id && d.path == path)
            .cloned())
    }

    async fn list_dentries(&self, workspace_id: &str, parent_path: &str) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && d.parent_path == parent_path)
            .cloned()
            .collect())
    }

    async fn list_children_capped(
        &self,
        workspace_id: &str,
        parent_path: &str,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let mut st = self.state.lock().unwrap();
        st.children_capped_limits.push(limit);
        let mut rows: Vec<Dentry> = st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && d.parent_path == parent_path)
            .cloned()
            .collect();
        // Mirror the SQL: ORDER BY is_dir DESC, path — then LIMIT.
        rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
        rows.truncate(limit);
        Ok(rows)
    }

    async fn count_files_by_top_level(
        &self,
        workspace_id: &str,
    ) -> Result<HashMap<String, i64>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .top_level_counts
            .clone()
            .into_iter()
            .filter(|_| !workspace_id.is_empty())
            .collect())
    }

    async fn list_dentries_under_page(
        &self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        let mut all: Vec<Dentry> = if path_prefix == "/" {
            st.dentries
                .iter()
                .filter(|d| d.workspace_id == workspace_id)
                .cloned()
                .collect()
        } else {
            let prefix = format!("{path_prefix}/");
            st.dentries
                .iter()
                .filter(|d| d.workspace_id == workspace_id && d.path.starts_with(&prefix))
                .cloned()
                .collect()
        };
        all.sort_by(|a, b| a.path.cmp(&b.path));
        if let Some(after) = after_path {
            all.retain(|d| d.path.as_str() > after);
        }
        all.truncate(limit);
        Ok(all)
    }

    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>> {
        let st = self.state.lock().unwrap();
        Ok(st.files.iter().find(|f| f.id == file_id).cloned())
    }

    async fn get_files_batch(&self, file_ids: &[String]) -> Result<Vec<FileRecord>> {
        let mut st = self.state.lock().unwrap();
        st.batch_id_counts.push(("files_batch", file_ids.len()));
        Ok(st
            .files
            .iter()
            .filter(|f| file_ids.iter().any(|id| id == &f.id))
            .cloned()
            .collect())
    }

    async fn get_file_content(&self, file_id: &str) -> Result<Option<String>> {
        let st = self.state.lock().unwrap();
        Ok(st.file_contents.get(file_id).cloned())
    }

    async fn get_file_blob(&self, file_id: &str) -> Result<Option<Vec<u8>>> {
        let st = self.state.lock().unwrap();
        Ok(st.file_blobs.get(file_id).cloned())
    }

    async fn get_file_extract(&self, file_id: &str) -> Result<Option<FileExtract>> {
        let st = self.state.lock().unwrap();
        Ok(st.file_extracts.get(file_id).cloned())
    }

    async fn upsert_file_extract(&self, extract: &FileExtract) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_extracts.insert(extract.file_id.clone(), extract.clone());
        Ok(())
    }

    async fn delete_file_extract(&self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_extracts.remove(file_id);
        Ok(())
    }

    async fn get_file_chunks(
        &self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let st = self.state.lock().unwrap();
        let mut chunks: Vec<FileChunk> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file_id)
            .cloned()
            .collect();
        chunks.sort_by_key(|c| c.chunk_index);
        Ok(filter_overlap(chunks, start_line, end_line))
    }

    async fn insert_dentry_ignore(&self, dentry: &Dentry) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let exists = st
            .dentries
            .iter()
            .any(|d| d.workspace_id == dentry.workspace_id && d.path == dentry.path);
        if !exists {
            st.dentries.push(dentry.clone());
        }
        Ok(())
    }

    async fn get_dentry_path_by_file_id(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<String>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .find(|d| d.workspace_id == workspace_id && d.file_id.as_deref() == Some(file_id))
            .map(|d| d.path.clone()))
    }

    async fn get_dentry_paths_by_file_ids(
        &self,
        workspace_id: &str,
        file_ids: &[String],
    ) -> Result<HashMap<String, DentryPathRef>> {
        // Mirror the MySQL impl: smallest path wins per file_id so
        // copy-alias attribution is deterministic.
        let st = self.state.lock().unwrap();
        let mut sorted: Vec<&Dentry> = st
            .dentries
            .iter()
            .filter(|d| {
                d.workspace_id == workspace_id
                    && d.file_id.as_ref().is_some_and(|f| file_ids.contains(f))
            })
            .collect();
        // Mirror the MySQL `ORDER BY path, id` (id tie-break for
        // collation-equal aliases; Rust String cmp is binary so the id key
        // only matters for exact duplicates, which the schema forbids).
        sorted.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
        let mut map = HashMap::new();
        for d in sorted {
            let fid = d.file_id.clone().expect("filtered on Some");
            map.entry(fid).or_insert(DentryPathRef {
                dentry_id: d.id.clone(),
                path: d.path.clone(),
            });
        }
        Ok(map)
    }

    async fn sum_bytes_by_child(
        &self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<HashMap<String, i64>> {
        // Mirror the MySQL shape: group files under parent by first
        // segment below it, summing file sizes.
        let st = self.state.lock().unwrap();
        let prefix = if parent_path == "/" {
            "/".to_string()
        } else {
            format!("{parent_path}/")
        };
        let sizes: HashMap<&str, i64> =
            st.files.iter().map(|f| (f.id.as_str(), f.size_bytes)).collect();
        let mut map: HashMap<String, i64> = HashMap::new();
        for d in st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && !d.is_dir)
        {
            if let Some(rest) = d.path.strip_prefix(&prefix) {
                let child = rest.split('/').next().unwrap_or("").to_string();
                let sz = d
                    .file_id
                    .as_deref()
                    .and_then(|fid| sizes.get(fid).copied())
                    .unwrap_or(0);
                *map.entry(child).or_insert(0) += sz;
            }
        }
        Ok(map)
    }

    async fn get_dentry_paths_by_ids(
        &self,
        workspace_id: &str,
        dentry_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && dentry_ids.contains(&d.id))
            .map(|d| (d.id.clone(), d.path.clone()))
            .collect())
    }

    async fn upsert_doc_access_daily(&self, rows: &[DocAccessRow]) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if st.fail_doc_access_upsert {
            return Err(VedaError::Storage("mock armed to fail".into()));
        }
        st.doc_access_rows.extend_from_slice(rows);
        Ok(())
    }

    async fn query_doc_access(
        &self,
        workspace_id: &str,
        since: chrono::NaiveDate,
        order: DocAccessOrder,
        limit: usize,
    ) -> Result<Vec<api::DocAccessEntry>> {
        // Aggregate captured rows joined against live dentries, mirroring
        // the MySQL GROUP BY + INNER JOIN semantics.
        let st = self.state.lock().unwrap();
        let mut by_dentry: HashMap<String, (u64, u64)> = HashMap::new();
        for r in st
            .doc_access_rows
            .iter()
            .filter(|r| r.workspace_id == workspace_id && r.day >= since)
        {
            let e = by_dentry.entry(r.dentry_id.clone()).or_default();
            e.0 += r.search_hits;
            e.1 += r.reads;
        }
        let mut out: Vec<api::DocAccessEntry> = by_dentry
            .into_iter()
            .filter_map(|(dentry_id, (hits, reads))| {
                st.dentries
                    .iter()
                    .find(|d| d.id == dentry_id)
                    .map(|d| api::DocAccessEntry {
                        path: d.path.clone(),
                        search_hits: hits,
                        reads,
                    })
            })
            .collect();
        match order {
            DocAccessOrder::Reads => out.sort_by(|a, b| b.reads.cmp(&a.reads)),
            DocAccessOrder::SearchHits => out.sort_by(|a, b| b.search_hits.cmp(&a.search_hits)),
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn sweep_doc_access(&self, cutoff: chrono::NaiveDate) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let before = st.doc_access_rows.len();
        st.doc_access_rows.retain(|r| r.day >= cutoff);
        Ok((before - st.doc_access_rows.len()) as u64)
    }

    async fn query_fs_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FsEvent>> {
        // Mirror the MySQL impl's subtree semantics: `prefix` matches the dir
        // entry itself (`path == prefix`) or anything strictly under it
        // (`path` starts with `prefix + "/"`). Plain `starts_with(prefix)`
        // would leak across siblings — see mysql.rs comment.
        let st = self.state.lock().unwrap();
        let normalized_prefix = path_prefix.map(|p| p.trim_end_matches('/'));
        let mut events: Vec<FsEvent> = st
            .fs_events
            .iter()
            .filter(|e| {
                e.workspace_id == workspace_id
                    && e.id > since_id
                    && match normalized_prefix {
                        None => true,
                        Some("") => true, // "/" trimmed to ""
                        Some(p) => e.path == p || e.path.starts_with(&format!("{p}/")),
                    }
            })
            .cloned()
            .collect();
        events.sort_by_key(|e| e.id);
        events.truncate(limit);
        Ok(events)
    }

    async fn min_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .fs_events
            .iter()
            .filter(|e| e.workspace_id == workspace_id)
            .map(|e| e.id)
            .min())
    }

    async fn max_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .fs_events
            .iter()
            .filter(|e| e.workspace_id == workspace_id)
            .map(|e| e.id)
            .max())
    }

    async fn prune_fs_events_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let before = st.fs_events.len();
        st.fs_events.retain(|e| e.created_at >= cutoff);
        Ok((before - st.fs_events.len()) as u64)
    }

    async fn insert_fs_event_direct(&self, event: &FsEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let mut e = event.clone();
        // Mirror the tx-flavored insert: id=0 means "assign one" so callers
        // can leave the field defaulted.
        if e.id == 0 {
            e.id = st.fs_events.iter().map(|x| x.id).max().unwrap_or(0) + 1;
        }
        st.fs_events.push(e);
        Ok(())
    }

    async fn storage_stats(&self, workspace_id: &str) -> Result<StorageStats> {
        let st = self.state.lock().unwrap();
        let mut total_files: i64 = 0;
        let mut total_dirs: i64 = 0;
        let mut total_bytes: i64 = 0;
        for d in &st.dentries {
            if d.workspace_id != workspace_id {
                continue;
            }
            if d.is_dir {
                total_dirs += 1;
            } else {
                total_files += 1;
            }
        }
        for f in &st.files {
            if f.workspace_id == workspace_id {
                total_bytes += f.size_bytes;
            }
        }
        Ok(StorageStats {
            total_files,
            total_directories: total_dirs,
            total_bytes,
        })
    }

    async fn update_file_content_hash(&self, file_id: &str, hash: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.files.iter_mut().find(|f| f.id == file_id) {
            f.last_embedded_content_hash = Some(hash.to_string());
        }
        Ok(())
    }

    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> {
        Ok(Box::new(MockTx {
            state: Arc::clone(&self.state),
        }))
    }

    async fn get_summary_by_file(&self, file_id: &str) -> Result<Option<FileSummary>> {
        Ok(self.state.lock().unwrap().file_summaries.get(file_id).cloned())
    }
    async fn get_summaries_by_file_ids(
        &self,
        file_ids: &[String],
    ) -> Result<HashMap<String, FileSummary>> {
        let mut st = self.state.lock().unwrap();
        st.batch_id_counts.push(("summaries_by_file", file_ids.len()));
        Ok(file_ids
            .iter()
            .filter_map(|id| st.file_summaries.get(id).map(|s| (id.clone(), s.clone())))
            .collect())
    }
    async fn get_summary_by_dentry(&self, dentry_id: &str) -> Result<Option<FileSummary>> {
        Ok(self.state.lock().unwrap().dir_summaries.get(dentry_id).cloned())
    }
    async fn get_summaries_by_dentry_ids(
        &self,
        dentry_ids: &[String],
    ) -> Result<HashMap<String, FileSummary>> {
        let mut st = self.state.lock().unwrap();
        st.batch_id_counts.push(("summaries_by_dentry", dentry_ids.len()));
        Ok(dentry_ids
            .iter()
            .filter_map(|id| st.dir_summaries.get(id).map(|s| (id.clone(), s.clone())))
            .collect())
    }
    async fn list_ready_summary_keys(
        &self,
        _workspace_id: &str,
    ) -> Result<(
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    )> {
        Ok((
            std::collections::HashSet::new(),
            std::collections::HashSet::new(),
        ))
    }
    async fn upsert_summary(&self, _summary: &FileSummary) -> Result<()> {
        Ok(())
    }
    async fn delete_summary_by_file(&self, _file_id: &str) -> Result<()> {
        Ok(())
    }
    async fn delete_summary_by_dentry(&self, _dentry_id: &str) -> Result<()> {
        Ok(())
    }
    async fn list_child_summaries(&self, _ws: &str, _parent: &str) -> Result<Vec<FileSummary>> {
        Ok(vec![])
    }
}

// ── In-memory MetadataTx ───────────────────────────────

pub struct MockTx {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl MetadataTx for MockTx {
    async fn get_dentry(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .find(|d| d.workspace_id == workspace_id && d.path == path)
            .cloned())
    }

    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.dentries.push(dentry.clone());
        Ok(())
    }

    async fn update_dentry_file_id(
        &mut self,
        workspace_id: &str,
        path: &str,
        file_id: &str,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st
            .dentries
            .iter_mut()
            .find(|d| d.workspace_id == workspace_id && d.path == path)
        {
            d.file_id = Some(file_id.to_string());
        }
        Ok(())
    }

    async fn delete_dentry(&mut self, workspace_id: &str, path: &str) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let before = st.dentries.len();
        st.dentries
            .retain(|d| !(d.workspace_id == workspace_id && d.path == path));
        Ok((before - st.dentries.len()) as u64)
    }

    async fn list_dentries_under_page(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        let mut all: Vec<Dentry> = st
            .dentries
            .iter()
            .filter(|d| {
                d.workspace_id == workspace_id && d.path.starts_with(&format!("{path_prefix}/"))
            })
            .cloned()
            .collect();
        all.sort_by(|a, b| a.path.cmp(&b.path));
        if let Some(after) = after_path {
            all.retain(|d| d.path.as_str() > after);
        }
        all.truncate(limit);
        Ok(all)
    }

    async fn delete_dentries_under(
        &mut self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let before = st.dentries.len();
        st.dentries.retain(|d| {
            !(d.workspace_id == workspace_id && d.path.starts_with(&format!("{parent_path}/")))
        });
        Ok((before - st.dentries.len()) as u64)
    }

    async fn rename_dentry(
        &mut self,
        workspace_id: &str,
        old_path: &str,
        new_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(d) = st
            .dentries
            .iter_mut()
            .find(|d| d.workspace_id == workspace_id && d.path == old_path)
        {
            d.path = new_path.to_string();
            d.parent_path = new_parent.to_string();
            d.name = new_name.to_string();
        }
        Ok(())
    }

    async fn rename_dentries_under(
        &mut self,
        workspace_id: &str,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<u64> {
        let mut st = self.state.lock().unwrap();
        let prefix_with_slash = format!("{old_prefix}/");
        let mut count = 0u64;
        for d in st.dentries.iter_mut() {
            if d.workspace_id == workspace_id && d.path.starts_with(&prefix_with_slash) {
                d.path = format!("{new_prefix}{}", &d.path[old_prefix.len()..]);
                d.parent_path = format!("{new_prefix}{}", &d.parent_path[old_prefix.len()..]);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>> {
        let st = self.state.lock().unwrap();
        Ok(st.files.iter().find(|f| f.id == file_id).cloned())
    }

    async fn insert_file(&mut self, file: &FileRecord) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.files.push(file.clone());
        Ok(())
    }

    async fn update_file_revision(
        &mut self,
        file_id: &str,
        expected_rev: i32,
        new_rev: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
        mime_type: &str,
        source_type: SourceType,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.files.iter_mut().find(|f| f.id == file_id) {
            if f.revision != expected_rev {
                return Err(VedaError::PreconditionFailed(format!(
                    "file {file_id} revision mismatch (expected {expected_rev}, actual {})",
                    f.revision
                )));
            }
            f.revision = new_rev;
            f.size_bytes = size_bytes;
            f.checksum_sha256 = checksum.to_string();
            f.line_count = line_count;
            f.storage_type = storage_type;
            f.mime_type = mime_type.to_string();
            f.source_type = source_type;
            f.last_embedded_content_hash = None;
            f.updated_at = chrono::Utc::now();
        }
        Ok(())
    }

    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32> {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.files.iter_mut().find(|f| f.id == file_id) {
            f.ref_count -= 1;
            return Ok(f.ref_count);
        }
        Ok(0)
    }

    async fn increment_ref_count(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.files.iter_mut().find(|f| f.id == file_id) {
            f.ref_count += 1;
        }
        Ok(())
    }

    async fn delete_file(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.files.retain(|f| f.id != file_id);
        Ok(())
    }

    async fn get_file_content(&mut self, file_id: &str) -> Result<Option<String>> {
        let st = self.state.lock().unwrap();
        Ok(st.file_contents.get(file_id).cloned())
    }

    async fn get_file_chunks(
        &mut self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let st = self.state.lock().unwrap();
        let mut chunks: Vec<_> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file_id)
            .cloned()
            .collect();
        chunks.sort_by_key(|c| c.chunk_index);
        Ok(filter_overlap(chunks, start_line, end_line))
    }

    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_contents
            .insert(file_id.to_string(), content.to_string());
        Ok(())
    }

    async fn delete_file_content(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_contents.remove(file_id);
        Ok(())
    }

    async fn insert_file_blob(&mut self, file_id: &str, data: &[u8]) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_blobs.insert(file_id.to_string(), data.to_vec());
        Ok(())
    }

    async fn delete_file_blob(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_blobs.remove(file_id);
        Ok(())
    }

    async fn delete_file_extract(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_extracts.remove(file_id);
        Ok(())
    }

    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_chunks.extend(chunks.iter().cloned());
        Ok(())
    }

    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_chunks.retain(|c| c.file_id != file_id);
        Ok(())
    }

    async fn delete_file_chunks_from(
        &mut self,
        file_id: &str,
        from_chunk_index: i32,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.file_chunks
            .retain(|c| !(c.file_id == file_id && c.chunk_index >= from_chunk_index));
        Ok(())
    }

    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.outbox.push(event.clone());
        Ok(())
    }

    async fn try_insert_outbox_for_file(
        &mut self,
        event: &OutboxEvent,
        file_id: &str,
    ) -> Result<bool> {
        // Only dedup against `Pending`. Processing tasks are already in
        // flight against an older snapshot — letting a new event through
        // ensures the latest content gets embedded after the in-flight
        // task completes. See try_insert_outbox_for_file in mysql.rs for
        // the full rationale.
        let mut st = self.state.lock().unwrap();
        let exists = st.outbox.iter().any(|e| {
            e.event_type == event.event_type
                && e.workspace_id == event.workspace_id
                && matches!(e.status, OutboxStatus::Pending)
                && e.payload.get("file_id").and_then(|v| v.as_str()) == Some(file_id)
        });
        if exists {
            return Ok(false);
        }
        st.outbox.push(event.clone());
        Ok(true)
    }

    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let mut e = event.clone();
        // Mirror MySQL's auto_increment: callers pass id=0 for "assign one".
        // Without this, query_fs_events(since_id=0) sees nothing because every
        // mocked event has id=0 and the strict `id > since_id` filter trips.
        if e.id == 0 {
            e.id = st.fs_events.iter().map(|x| x.id).max().unwrap_or(0) + 1;
        }
        st.fs_events.push(e);
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

use std::sync::Arc;

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use veda_types::*;

use crate::path;
use crate::service::retry_on_deadlock;
use crate::store::{MetadataStore, MetadataTx};

const INLINE_THRESHOLD: i64 = 256 * 1024;
const CHUNK_SIZE: usize = 256 * 1024;
const MAX_FILE_BYTES: i64 = 50 * 1024 * 1024;
const MAX_LINE_RANGE: i32 = 100_000;

/// Per-write precomputed metadata: full-content hash, size, line count,
/// and either an inline string or chunk list ready to persist.
///
/// Produced in a single pass over the bytes via [`compute_write_meta`].
struct WriteMeta {
    sha256: String,
    size: i64,
    line_count: i32,
    payload: WritePayload,
}

enum WritePayload {
    /// size <= INLINE_THRESHOLD → stored as a single row.
    Inline { content: String },
    /// size > INLINE_THRESHOLD → chunked; `file_id` is empty and must be set by caller.
    Chunked { chunks: Vec<FileChunk> },
}

impl WriteMeta {
    fn storage_type(&self) -> StorageType {
        match self.payload {
            WritePayload::Inline { .. } => StorageType::Inline,
            WritePayload::Chunked { .. } => StorageType::Chunked,
        }
    }
}

/// Async wrapper that runs CPU-heavy hashing + chunk-splitting on the blocking
/// thread pool so it does not starve the tokio runtime worker threads.
async fn compute_write_meta(content: String) -> Result<WriteMeta> {
    tokio::task::spawn_blocking(move || {
        compute_write_meta_blocking(content, CHUNK_SIZE, INLINE_THRESHOLD)
    })
    .await
    .map_err(|e| VedaError::Internal(format!("hash task join failed: {e}")))
}

/// Single-pass metadata computation.
///
/// Walks `content` once per chunk slice to produce:
/// - full-content sha256 (fed chunk-by-chunk into one hasher),
/// - per-chunk sha256 for incremental-append bookkeeping,
/// - chunk boundaries aligned to '\n' so `start_line` is unique per chunk
///   (falling back to the next '\n' when a single line exceeds `chunk_size`).
fn compute_write_meta_blocking(
    content: String,
    chunk_size: usize,
    inline_threshold: i64,
) -> WriteMeta {
    let size = content.len() as i64;
    let bytes = content.as_bytes();

    // Inline: one hash pass + one line-count pass (both cache-hot on ≤256 KB).
    if size <= inline_threshold {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha256 = format!("{:x}", hasher.finalize());
        let line_count = content.lines().count().min(i32::MAX as usize) as i32;
        return WriteMeta {
            sha256,
            size,
            line_count,
            payload: WritePayload::Inline { content },
        };
    }

    let (chunks, full_sha, line_count) = split_and_hash(bytes, chunk_size, 0, 1);

    WriteMeta {
        sha256: full_sha,
        size,
        line_count,
        payload: WritePayload::Chunked { chunks },
    }
}

/// Split `bytes` into '\n'-aligned chunks, computing each chunk's sha256,
/// the full-buffer sha256 (one combined hasher), and the total line count
/// under `str::lines()` semantics. Chunks carry empty `file_id` — the caller
/// fills it in before persisting.
fn split_and_hash(
    bytes: &[u8],
    chunk_size: usize,
    start_chunk_index: i32,
    start_line: i32,
) -> (Vec<FileChunk>, String, i32) {
    let mut full_hasher = Sha256::new();
    let mut chunks: Vec<FileChunk> = Vec::new();
    let mut offset = 0usize;
    let mut chunk_index = start_chunk_index;
    let mut current_line = start_line;
    let chunk_size = chunk_size.max(1);

    while offset < bytes.len() {
        let mut end = (offset + chunk_size).min(bytes.len());
        if end < bytes.len() {
            if let Some(nl) = bytes[offset..end].iter().rposition(|&b| b == b'\n') {
                end = offset + nl + 1;
            } else {
                // A single line exceeds `chunk_size`; extend to the next '\n'
                // (or EOF). Preserves the invariant that every non-final chunk
                // ends on '\n', which keeps `start_line` unique across chunks.
                match bytes[end..].iter().position(|&b| b == b'\n') {
                    Some(nl) => end += nl + 1,
                    None => end = bytes.len(),
                }
            }
        }

        let slice = &bytes[offset..end];
        full_hasher.update(slice);
        let mut chunk_hasher = Sha256::new();
        chunk_hasher.update(slice);
        let chunk_sha256 = format!("{:x}", chunk_hasher.finalize());
        let nl_count = slice.iter().filter(|&&b| b == b'\n').count() as i32;
        // SAFETY: chunk boundaries land on '\n' bytes or EOF — always on a
        // UTF-8 char boundary for valid UTF-8 input.
        let chunk_content = std::str::from_utf8(slice)
            .expect("chunk boundary must align with UTF-8")
            .to_string();

        chunks.push(FileChunk {
            file_id: String::new(),
            chunk_index,
            start_line: current_line,
            line_count: nl_count,
            byte_len: slice.len() as i32,
            chunk_sha256,
            content: chunk_content,
        });

        current_line += nl_count;
        chunk_index += 1;
        offset = end;
    }

    let full_sha = format!("{:x}", full_hasher.finalize());
    let trailing_partial = bytes.last().map(|&b| b != b'\n').unwrap_or(false);
    let newline_total: i32 = chunks.iter().map(|c| c.line_count).sum();
    let line_count = newline_total + if trailing_partial { 1 } else { 0 };

    (chunks, full_sha, line_count)
}

/// Result of an incremental chunked append: new full-content sha256, total
/// line_count, and the tail chunks that replace the old last chunk onwards.
struct AppendMeta {
    sha256: String,
    line_count: i32,
    tail_chunks: Vec<FileChunk>,
}

/// Compute new file metadata given the tail rewrite.
///
/// `pre_tail_contents` is the ordered content of every chunk before
/// `last_chunk_idx`; the last chunk's content is passed separately already
/// merged into `tail`. Runs on the blocking thread pool.
fn compute_append_meta_blocking(
    pre_tail_contents: &[String],
    before_line_count: i32,
    last_chunk_idx: i32,
    tail_start_line: i32,
    tail: String,
    chunk_size: usize,
) -> AppendMeta {
    let mut hasher = Sha256::new();
    for c in pre_tail_contents {
        hasher.update(c.as_bytes());
    }
    hasher.update(tail.as_bytes());
    let sha256 = format!("{:x}", hasher.finalize());

    let (tail_chunks, _tail_only_sha, _tail_lines_with_trailing) =
        split_and_hash(tail.as_bytes(), chunk_size, last_chunk_idx, tail_start_line);

    let tail_newlines: i32 = tail_chunks.iter().map(|c| c.line_count).sum();
    let trailing_partial = tail.as_bytes().last().map(|&b| b != b'\n').unwrap_or(false);
    let line_count = before_line_count + tail_newlines + if trailing_partial { 1 } else { 0 };

    AppendMeta {
        sha256,
        line_count,
        tail_chunks,
    }
}

pub struct FsService {
    meta: Arc<dyn MetadataStore>,
}

impl FsService {
    pub fn new(meta: Arc<dyn MetadataStore>) -> Self {
        Self { meta }
    }

    pub async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>> {
        self.meta.get_file(file_id).await
    }

    pub async fn write_file(
        &self,
        workspace_id: &str,
        raw_path: &str,
        content: &str,
        expected_revision: Option<i32>,
        if_none_match_sha256: Option<&str>,
    ) -> Result<api::WriteFileResponse> {
        let size = content.len() as i64;
        if size > MAX_FILE_BYTES {
            return Err(VedaError::QuotaExceeded(format!(
                "file size {}MB exceeds {}MB limit",
                size / 1024 / 1024,
                MAX_FILE_BYTES / 1024 / 1024,
            )));
        }

        // Fast-path precheck: if the client asserts an exact sha256 and it
        // matches the currently stored one, we can skip hashing the body and
        // the whole DB write altogether.
        if let Some(client_hash) = if_none_match_sha256 {
            let norm = path::normalize(raw_path)?;
            if let Some(existing) = self.meta.get_dentry(workspace_id, &norm).await? {
                if !existing.is_dir {
                    if let Some(fid) = existing.file_id.as_deref() {
                        if let Some(f) = self.meta.get_file(fid).await? {
                            if f.checksum_sha256 == client_hash {
                                // Still enforce If-Match revision when set.
                                if let Some(expected) = expected_revision {
                                    if f.revision != expected {
                                        return Err(VedaError::PreconditionFailed(format!(
                                            "revision mismatch: expected {expected}, actual {}",
                                            f.revision
                                        )));
                                    }
                                }
                                return Ok(api::WriteFileResponse {
                                    file_id: fid.to_string(),
                                    revision: f.revision,
                                    content_unchanged: true,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Hash + split chunks once, off the tokio runtime threads.
        let meta = compute_write_meta(content.to_string()).await?;

        let ws = workspace_id.to_string();
        let p = raw_path.to_string();
        let meta = Arc::new(meta);
        retry_on_deadlock(|| self.write_file_once(&ws, &p, Arc::clone(&meta), expected_revision))
            .await
    }

    async fn write_file_once(
        &self,
        workspace_id: &str,
        raw_path: &str,
        meta: Arc<WriteMeta>,
        expected_revision: Option<i32>,
    ) -> Result<api::WriteFileResponse> {
        let norm = path::normalize(raw_path)?;

        ensure_parents(&*self.meta, workspace_id, &norm).await?;

        let mut tx = self.meta.begin_tx().await?;
        let existing = tx.get_dentry(workspace_id, &norm).await?;

        if let Some(ref dentry) = existing {
            if dentry.is_dir {
                return Err(VedaError::AlreadyExists(format!("{norm} is a directory")));
            }
            if let Some(ref fid) = dentry.file_id {
                let file = tx.get_file(fid).await?;
                if let Some(f) = file {
                    if let Some(expected) = expected_revision {
                        if f.revision != expected {
                            return Err(VedaError::PreconditionFailed(format!(
                                "revision mismatch: expected {expected}, actual {}",
                                f.revision
                            )));
                        }
                    }

                    if f.checksum_sha256 == meta.sha256 {
                        tx.commit().await?;
                        return Ok(api::WriteFileResponse {
                            file_id: fid.clone(),
                            revision: f.revision,
                            content_unchanged: true,
                        });
                    }

                    return finalize_full_rewrite(tx, workspace_id, &norm, &f, fid, &meta)
                        .await;
                }
            }
        }

        // Caller expects an existing file but it doesn't exist
        if let Some(expected) = expected_revision {
            if expected != 0 {
                return Err(VedaError::PreconditionFailed(format!(
                    "file not found; expected revision {expected}"
                )));
            }
        }

        let file_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let file = FileRecord {
            id: file_id.clone(),
            workspace_id: workspace_id.to_string(),
            size_bytes: meta.size,
            mime_type: "text/plain".to_string(),
            storage_type: meta.storage_type(),
            source_type: SourceType::Text,
            line_count: Some(meta.line_count),
            checksum_sha256: meta.sha256.clone(),
            revision: 1,
            ref_count: 1,
            created_at: now,
            updated_at: now,
        };
        tx.insert_file(&file).await?;
        persist_write_meta(&mut *tx, &file_id, &meta).await?;

        if existing.is_some() {
            tx.update_dentry_file_id(workspace_id, &norm, &file_id)
                .await?;
        } else {
            let dentry = Dentry {
                id: Uuid::new_v4().to_string(),
                workspace_id: workspace_id.to_string(),
                parent_path: path::parent(&norm).to_string(),
                name: path::filename(&norm).to_string(),
                path: norm.clone(),
                file_id: Some(file_id.clone()),
                is_dir: false,
                created_at: now,
                updated_at: now,
            };
            tx.insert_dentry(&dentry).await?;
        }

        let outbox = make_outbox(workspace_id, OutboxEventType::ChunkSync, &file_id);
        tx.insert_outbox(&outbox).await?;

        let evt = make_fs_event(workspace_id, FsEventType::Create, &norm, Some(&file_id));
        tx.insert_fs_event(&evt).await?;

        tx.commit().await?;

        Ok(api::WriteFileResponse {
            file_id,
            revision: 1,
            content_unchanged: false,
        })
    }

    async fn resolve_file(
        &self,
        workspace_id: &str,
        raw_path: &str,
    ) -> Result<(String, FileRecord)> {
        let norm = path::normalize(raw_path)?;
        let dentry = self
            .meta
            .get_dentry(workspace_id, &norm)
            .await?
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;
        if dentry.is_dir {
            return Err(VedaError::InvalidPath(format!("{norm} is a directory")));
        }
        let file_id = dentry
            .file_id
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;
        let file = self
            .meta
            .get_file(&file_id)
            .await?
            .ok_or(VedaError::NotFound(norm))?;
        Ok((file_id, file))
    }

    pub async fn read_file(&self, workspace_id: &str, raw_path: &str) -> Result<String> {
        let (file_id, file) = self.resolve_file(workspace_id, raw_path).await?;
        match file.storage_type {
            StorageType::Inline => self
                .meta
                .get_file_content(&file_id)
                .await?
                .ok_or_else(|| VedaError::NotFound(format!("content for {file_id}"))),
            StorageType::Chunked => {
                let chunks = self.meta.get_file_chunks(&file_id, None, None).await?;
                Ok(chunks.into_iter().map(|c| c.content).collect::<String>())
            }
        }
    }

    /// Query file system events since a given ID.
    pub async fn query_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        limit: usize,
    ) -> Result<Vec<FsEvent>> {
        self.meta
            .query_fs_events(workspace_id, since_id, None, limit)
            .await
    }

    /// Read a byte range from a file. Returns (data, total_size).
    pub async fn read_file_range(
        &self,
        workspace_id: &str,
        raw_path: &str,
        offset: u64,
        length: u64,
    ) -> Result<(Vec<u8>, u64)> {
        let content = self.read_file(workspace_id, raw_path).await?;
        let bytes = content.as_bytes();
        let total = bytes.len() as u64;

        if offset >= total {
            return Ok((Vec::new(), total));
        }

        let end = std::cmp::min(offset.saturating_add(length), total) as usize;
        Ok((bytes[offset as usize..end].to_vec(), total))
    }

    pub async fn read_file_lines(
        &self,
        workspace_id: &str,
        raw_path: &str,
        start: i32,
        end: i32,
    ) -> Result<String> {
        if start < 1 || end < start {
            return Err(VedaError::InvalidInput(format!(
                "invalid line range {start}..{end}"
            )));
        }
        if end - start + 1 > MAX_LINE_RANGE {
            return Err(VedaError::InvalidInput(format!(
                "line range exceeds {MAX_LINE_RANGE}"
            )));
        }

        let (file_id, file) = self.resolve_file(workspace_id, raw_path).await?;

        // early-return when the requested range lies beyond EOF
        if let Some(lc) = file.line_count {
            if start > lc {
                return Ok(String::new());
            }
        }
        let effective_end = file.line_count.map_or(end, |lc| end.min(lc));
        let skip = (start - 1) as usize;
        let take = (effective_end - start + 1) as usize;

        match file.storage_type {
            StorageType::Inline => {
                let content = self
                    .meta
                    .get_file_content(&file_id)
                    .await?
                    .ok_or_else(|| VedaError::NotFound(format!("content for {file_id}")))?;
                Ok(join_lines(content.lines().skip(skip).take(take)))
            }
            StorageType::Chunked => {
                // get_file_chunks returns chunks overlapping [start, effective_end];
                // the first chunk may contain lines before `start`, so rebase the skip.
                let chunks = self
                    .meta
                    .get_file_chunks(&file_id, Some(start), Some(effective_end))
                    .await?;
                let Some(first) = chunks.first() else {
                    return Ok(String::new());
                };
                let skip_in_chunks = (start - first.start_line).max(0) as usize;
                Ok(join_lines(
                    chunks
                        .iter()
                        .flat_map(|c| c.content.lines())
                        .skip(skip_in_chunks)
                        .take(take),
                ))
            }
        }
    }

    pub async fn list_dir(&self, workspace_id: &str, raw_path: &str) -> Result<Vec<api::DirEntry>> {
        let norm = path::normalize(raw_path)?;
        if norm != "/" {
            let dentry = self
                .meta
                .get_dentry(workspace_id, &norm)
                .await?
                .ok_or_else(|| VedaError::NotFound(norm.clone()))?;
            if !dentry.is_dir {
                return Err(VedaError::InvalidPath(format!("{norm} is not a directory")));
            }
        }
        let dentries = self.meta.list_dentries(workspace_id, &norm).await?;
        Ok(dentries
            .into_iter()
            .map(|d| api::DirEntry {
                name: d.name,
                path: d.path,
                is_dir: d.is_dir,
                size_bytes: None,
                mime_type: None,
            })
            .collect())
    }

    /// Recursively list all dentries under a directory.
    /// `max_entries` caps the total number of collected dentries to prevent OOM.
    pub async fn list_dir_recursive(
        &self,
        workspace_id: &str,
        raw_path: &str,
        max_entries: usize,
    ) -> Result<Vec<Dentry>> {
        let norm = path::normalize(raw_path)?;
        let all = self.meta.list_dentries_under(workspace_id, &norm).await?;
        if all.len() > max_entries {
            return Err(VedaError::QuotaExceeded(format!(
                "directory listing exceeded {} entries",
                max_entries
            )));
        }
        Ok(all)
    }

    /// Match files using a glob pattern. Returns matching dentries.
    /// Pattern supports `*` (any chars except `/`), `?` (single char), `**` (recursive).
    pub async fn glob_files(
        &self,
        workspace_id: &str,
        pattern: &str,
        max_matches: usize,
    ) -> Result<Vec<Dentry>> {
        let prefix = glob_fixed_prefix(pattern);
        let all = self
            .list_dir_recursive(workspace_id, &prefix, max_matches)
            .await?;
        Ok(all
            .into_iter()
            .filter(|d| !d.is_dir && glob_match(pattern, &d.path))
            .collect())
    }

    pub async fn stat(&self, workspace_id: &str, raw_path: &str) -> Result<api::FileInfo> {
        let norm = path::normalize(raw_path)?;
        // Root directory has no dentry row; synthesize a virtual one
        // to stay consistent with list_dir's root handling.
        if norm == "/" {
            let now = Utc::now();
            return Ok(api::FileInfo {
                path: "/".to_string(),
                file_id: None,
                is_dir: true,
                size_bytes: None,
                mime_type: None,
                revision: None,
                checksum: None,
                created_at: now,
                updated_at: now,
            });
        }
        let dentry = self
            .meta
            .get_dentry(workspace_id, &norm)
            .await?
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;

        if dentry.is_dir {
            return Ok(api::FileInfo {
                path: dentry.path,
                file_id: None,
                is_dir: true,
                size_bytes: None,
                mime_type: None,
                revision: None,
                checksum: None,
                created_at: dentry.created_at,
                updated_at: dentry.updated_at,
            });
        }

        let file = match &dentry.file_id {
            Some(fid) => self.meta.get_file(fid).await?,
            None => None,
        };

        Ok(api::FileInfo {
            path: dentry.path,
            file_id: dentry.file_id,
            is_dir: false,
            size_bytes: file.as_ref().map(|f| f.size_bytes),
            mime_type: file.as_ref().map(|f| f.mime_type.clone()),
            revision: file.as_ref().map(|f| f.revision),
            checksum: file.as_ref().map(|f| f.checksum_sha256.clone()),
            created_at: dentry.created_at,
            updated_at: dentry.updated_at,
        })
    }

    /// Delete a file or directory. Returns the number of deleted dentries.
    pub async fn delete(&self, workspace_id: &str, raw_path: &str) -> Result<u64> {
        let norm = path::normalize(raw_path)?;
        if norm == "/" {
            return Err(VedaError::InvalidPath("cannot delete root".to_string()));
        }

        let mut tx = self.meta.begin_tx().await?;
        let dentry = tx
            .get_dentry(workspace_id, &norm)
            .await?
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;

        let mut file_ids_to_cleanup: Vec<String> = Vec::new();
        let mut deleted_count: u64 = 1; // the target dentry itself
        let mut child_events: Vec<FsEvent> = Vec::new();

        if dentry.is_dir {
            let children = tx.list_dentries_under(workspace_id, &norm).await?;
            deleted_count += children.len() as u64;
            for child in &children {
                if let Some(ref fid) = child.file_id {
                    file_ids_to_cleanup.push(fid.clone());
                }
                child_events.push(make_fs_event(
                    workspace_id,
                    FsEventType::Delete,
                    &child.path,
                    child.file_id.as_deref(),
                ));
            }
            tx.delete_dentries_under(workspace_id, &norm).await?;
        }

        tx.delete_dentry(workspace_id, &norm).await?;

        if let Some(ref fid) = dentry.file_id {
            file_ids_to_cleanup.push(fid.clone());
        }

        for fid in &file_ids_to_cleanup {
            let remaining = tx.decrement_ref_count(fid).await?;
            if remaining <= 0 {
                tx.delete_file_content(fid).await?;
                tx.delete_file_chunks(fid).await?;
                let outbox = make_outbox(workspace_id, OutboxEventType::ChunkDelete, fid);
                tx.insert_outbox(&outbox).await?;
                tx.delete_file(fid).await?;
            }
        }

        let evt = make_fs_event(
            workspace_id,
            FsEventType::Delete,
            &norm,
            dentry.file_id.as_deref(),
        );
        tx.insert_fs_event(&evt).await?;
        if !child_events.is_empty() {
            tx.insert_fs_events(&child_events).await?;
        }
        tx.commit().await?;
        Ok(deleted_count)
    }

    pub async fn mkdir(&self, workspace_id: &str, raw_path: &str) -> Result<()> {
        let norm = path::normalize(raw_path)?;
        if norm == "/" {
            return Ok(());
        }

        let existing = self.meta.get_dentry(workspace_id, &norm).await?;
        if let Some(d) = existing {
            if d.is_dir {
                return Ok(());
            }
            return Err(VedaError::AlreadyExists(format!("{norm} exists as a file")));
        }

        ensure_parents(&*self.meta, workspace_id, &norm).await?;

        let now = Utc::now();
        let dentry = Dentry {
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace_id.to_string(),
            parent_path: path::parent(&norm).to_string(),
            name: path::filename(&norm).to_string(),
            path: norm.clone(),
            file_id: None,
            is_dir: true,
            created_at: now,
            updated_at: now,
        };
        self.meta.insert_dentry_ignore(&dentry).await
    }

    pub async fn copy_file(
        &self,
        workspace_id: &str,
        src_path: &str,
        dst_path: &str,
    ) -> Result<api::WriteFileResponse> {
        let src = path::normalize(src_path)?;
        let dst = path::normalize(dst_path)?;

        if src == dst {
            return Err(VedaError::InvalidInput(
                "source and destination are the same".to_string(),
            ));
        }

        ensure_parents(&*self.meta, workspace_id, &dst).await?;

        let mut tx = self.meta.begin_tx().await?;

        let src_dentry = match tx.get_dentry(workspace_id, &src).await? {
            Some(dentry) => dentry,
            None => {
                tx.rollback().await?;
                return Err(VedaError::NotFound(src.clone()));
            }
        };

        if src_dentry.is_dir {
            tx.rollback().await?;
            return Err(VedaError::InvalidInput(
                "cannot copy a directory".to_string(),
            ));
        }

        let file_id = match src_dentry.file_id.as_deref() {
            Some(fid) => fid,
            None => {
                tx.rollback().await?;
                return Err(VedaError::NotFound(src.clone()));
            }
        };

        let existing_dst = tx.get_dentry(workspace_id, &dst).await?;
        if let Some(ref d) = existing_dst {
            if d.is_dir {
                tx.rollback().await?;
                return Err(VedaError::AlreadyExists(format!("{dst} is a directory")));
            }
            if d.file_id.as_deref() == Some(file_id) {
                tx.commit().await?;
                let file = self
                    .meta
                    .get_file(file_id)
                    .await?
                    .ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;
                return Ok(api::WriteFileResponse {
                    file_id: file_id.to_string(),
                    revision: file.revision,
                    content_unchanged: true,
                });
            }
        }

        tx.increment_ref_count(file_id).await?;

        if let Some(d) = existing_dst {
            if let Some(ref old_fid) = d.file_id {
                let remaining = tx.decrement_ref_count(old_fid).await?;
                if remaining <= 0 {
                    tx.delete_file_content(old_fid).await?;
                    tx.delete_file_chunks(old_fid).await?;
                    let outbox = make_outbox(workspace_id, OutboxEventType::ChunkDelete, old_fid);
                    tx.insert_outbox(&outbox).await?;
                    tx.delete_file(old_fid).await?;
                }
            }
            tx.update_dentry_file_id(workspace_id, &dst, file_id)
                .await?;
        } else {
            let now = Utc::now();
            let dentry = Dentry {
                id: Uuid::new_v4().to_string(),
                workspace_id: workspace_id.to_string(),
                parent_path: path::parent(&dst).to_string(),
                name: path::filename(&dst).to_string(),
                path: dst.clone(),
                file_id: Some(file_id.to_string()),
                is_dir: false,
                created_at: now,
                updated_at: now,
            };
            tx.insert_dentry(&dentry).await?;
        }

        let evt = make_fs_event(workspace_id, FsEventType::Create, &dst, Some(file_id));
        tx.insert_fs_event(&evt).await?;

        tx.commit().await?;

        let file = self
            .meta
            .get_file(file_id)
            .await?
            .ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;

        Ok(api::WriteFileResponse {
            file_id: file_id.to_string(),
            revision: file.revision,
            content_unchanged: true,
        })
    }

    pub async fn append_file(
        &self,
        workspace_id: &str,
        raw_path: &str,
        content: &str,
    ) -> Result<api::WriteFileResponse> {
        let ws = workspace_id.to_string();
        let p = raw_path.to_string();
        let c = content.to_string();
        retry_on_deadlock(|| self.append_file_once(&ws, &p, &c)).await
    }

    async fn append_file_once(
        &self,
        workspace_id: &str,
        raw_path: &str,
        content: &str,
    ) -> Result<api::WriteFileResponse> {
        let norm = path::normalize(raw_path)?;
        let mut tx = self.meta.begin_tx().await?;
        let existing = tx.get_dentry(workspace_id, &norm).await?;

        // Fast-fail: append to a directory
        if let Some(ref d) = existing {
            if d.is_dir {
                return Err(VedaError::AlreadyExists(format!("{norm} is a directory")));
            }
        }

        // No existing file → append-as-create: reuse the write path directly.
        let dentry_with_file = existing
            .as_ref()
            .and_then(|d| d.file_id.as_deref().map(|fid| (d, fid)));
        let Some((_, fid)) = dentry_with_file else {
            tx.rollback().await.ok();
            let meta = compute_write_meta(content.to_string()).await?;
            let ws = workspace_id.to_string();
            let p = raw_path.to_string();
            let meta = Arc::new(meta);
            return retry_on_deadlock(|| self.write_file_once(&ws, &p, Arc::clone(&meta), None))
                .await;
        };

        let file = tx
            .get_file(fid)
            .await?
            .ok_or_else(|| VedaError::NotFound(fid.to_string()))?;

        let new_size = file.size_bytes + content.len() as i64;
        if new_size > MAX_FILE_BYTES {
            return Err(VedaError::QuotaExceeded(format!(
                "append would exceed {}MB limit (current {} + append {})",
                MAX_FILE_BYTES / 1024 / 1024,
                file.size_bytes,
                content.len()
            )));
        }

        if content.is_empty() {
            tx.commit().await?;
            return Ok(api::WriteFileResponse {
                file_id: fid.to_string(),
                revision: file.revision,
                content_unchanged: true,
            });
        }

        // Shared file (ref_count > 1) or transition from inline to chunked
        // would make in-place delta writes unsafe or complex. Fall back to a
        // full-rewrite path: read all existing bytes, concat with the new
        // content, and go through the regular single-pass pipeline.
        let old_is_chunked = matches!(file.storage_type, StorageType::Chunked);
        let will_stay_chunked = old_is_chunked && new_size > INLINE_THRESHOLD;
        let can_incremental = file.ref_count == 1 && will_stay_chunked;

        if !can_incremental {
            let mut merged = read_full_content(&mut *tx, fid, &file).await?;
            merged.push_str(content);
            let meta = compute_write_meta(merged).await?;

            if meta.sha256 == file.checksum_sha256 {
                tx.commit().await?;
                return Ok(api::WriteFileResponse {
                    file_id: fid.to_string(),
                    revision: file.revision,
                    content_unchanged: true,
                });
            }

            return finalize_full_rewrite(tx, workspace_id, &norm, &file, fid, &meta).await;
        }

        // Incremental chunked append: only re-chunk the tail, no full-file re-read.
        // We still need all chunk contents for the full-file SHA256 recomputation.
        let chunks = tx.get_file_chunks(fid, None, None).await?;
        let last = chunks
            .last()
            .ok_or_else(|| VedaError::Internal("chunked file missing last chunk".into()))?
            .clone();

        let fid_string = fid.to_string();
        let file_rev = file.revision;
        let last_chunk_idx = last.chunk_index;
        let last_start_line = last.start_line;

        let mut before_line_count = 0i32;
        let mut pre_tail_contents = Vec::new();
        for c in chunks {
            if c.chunk_index < last_chunk_idx {
                before_line_count += c.line_count;
                pre_tail_contents.push(c.content);
            }
        }
        let mut tail = String::with_capacity(last.content.len() + content.len());
        tail.push_str(&last.content);
        tail.push_str(content);

        let append_meta = tokio::task::spawn_blocking(move || {
            compute_append_meta_blocking(
                &pre_tail_contents,
                before_line_count,
                last_chunk_idx,
                last_start_line,
                tail,
                CHUNK_SIZE,
            )
        })
        .await
        .map_err(|e| VedaError::Internal(format!("append hash task join failed: {e}")))?;

        tx.delete_file_chunks_from(&fid_string, last_chunk_idx)
            .await?;
        let tail_with_fid: Vec<FileChunk> = append_meta
            .tail_chunks
            .into_iter()
            .map(|mut c| {
                c.file_id = fid_string.clone();
                c
            })
            .collect();
        tx.insert_file_chunks(&tail_with_fid).await?;

        let new_rev = file_rev + 1;
        tx.update_file_revision(
            &fid_string,
            file_rev,
            new_rev,
            new_size,
            &append_meta.sha256,
            Some(append_meta.line_count),
            StorageType::Chunked,
        )
        .await?;

        let outbox = make_outbox(workspace_id, OutboxEventType::ChunkSync, &fid_string);
        tx.insert_outbox(&outbox).await?;
        let evt = make_fs_event(workspace_id, FsEventType::Update, &norm, Some(&fid_string));
        tx.insert_fs_event(&evt).await?;
        tx.commit().await?;

        Ok(api::WriteFileResponse {
            file_id: fid_string,
            revision: new_rev,
            content_unchanged: false,
        })
    }

    pub async fn rename(&self, workspace_id: &str, src_path: &str, dst_path: &str) -> Result<()> {
        let src = path::normalize(src_path)?;
        let dst = path::normalize(dst_path)?;

        if src == dst {
            return Ok(());
        }

        ensure_parents(&*self.meta, workspace_id, &dst).await?;

        let mut tx = self.meta.begin_tx().await?;

        let src_dentry = tx
            .get_dentry(workspace_id, &src)
            .await?
            .ok_or_else(|| VedaError::NotFound(src.clone()))?;

        let dst_exists = tx.get_dentry(workspace_id, &dst).await?;
        if dst_exists.is_some() {
            tx.rollback().await?;
            return Err(VedaError::AlreadyExists(dst));
        }

        if src_dentry.is_dir && dst.starts_with(&format!("{src}/")) {
            tx.rollback().await?;
            return Err(VedaError::InvalidInput(
                "cannot move a directory into itself".to_string(),
            ));
        }

        tx.rename_dentry(
            workspace_id,
            &src,
            &dst,
            path::parent(&dst),
            path::filename(&dst),
        )
        .await?;

        let mut child_events: Vec<FsEvent> = Vec::new();
        if src_dentry.is_dir {
            let children = tx.list_dentries_under(workspace_id, &src).await?;
            for child in &children {
                let new_child_path = format!("{dst}{}", &child.path[src.len()..]);
                child_events.push(make_fs_event(
                    workspace_id,
                    FsEventType::Move,
                    &new_child_path,
                    child.file_id.as_deref(),
                ));
            }
            tx.rename_dentries_under(workspace_id, &src, &dst).await?;
        }

        let evt = make_fs_event(
            workspace_id,
            FsEventType::Move,
            &dst,
            src_dentry.file_id.as_deref(),
        );
        tx.insert_fs_event(&evt).await?;
        if !child_events.is_empty() {
            tx.insert_fs_events(&child_events).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────

fn join_lines<'a, I: Iterator<Item = &'a str>>(mut it: I) -> String {
    let Some(first) = it.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(first.len() + 64);
    out.push_str(first);
    for line in it {
        out.push('\n');
        out.push_str(line);
    }
    out
}

async fn cleanup_file_if_orphan(
    tx: &mut dyn MetadataTx,
    workspace_id: &str,
    file_id: &str,
) -> Result<()> {
    let remaining = tx.decrement_ref_count(file_id).await?;
    if remaining <= 0 {
        tx.delete_file_content(file_id).await?;
        tx.delete_file_chunks(file_id).await?;
        let outbox = make_outbox(workspace_id, OutboxEventType::ChunkDelete, file_id);
        tx.insert_outbox(&outbox).await?;
        tx.delete_file(file_id).await?;
    }
    Ok(())
}

/// Write a pre-computed [`WriteMeta`] under the given `file_id`.
///
/// For chunked payloads, the chunks' `file_id` is rewritten to the target id.
async fn persist_write_meta(
    tx: &mut dyn MetadataTx,
    file_id: &str,
    meta: &WriteMeta,
) -> Result<()> {
    match &meta.payload {
        WritePayload::Inline { content } => tx.insert_file_content(file_id, content).await,
        WritePayload::Chunked { chunks } => {
            let with_id: Vec<FileChunk> = chunks
                .iter()
                .map(|c| FileChunk {
                    file_id: file_id.to_string(),
                    ..c.clone()
                })
                .collect();
            tx.insert_file_chunks(&with_id).await
        }
    }
}

/// Read a file's full content regardless of storage type.
async fn read_full_content(
    tx: &mut dyn MetadataTx,
    file_id: &str,
    file: &FileRecord,
) -> Result<String> {
    Ok(match file.storage_type {
        StorageType::Inline => tx.get_file_content(file_id).await?.unwrap_or_default(),
        StorageType::Chunked => {
            let chunks = tx.get_file_chunks(file_id, None, None).await?;
            let total: usize = chunks.iter().map(|c| c.content.len()).sum();
            let mut s = String::with_capacity(total);
            for c in chunks {
                s.push_str(&c.content);
            }
            s
        }
    })
}

/// Apply a fresh [`WriteMeta`] to an existing dentry, handling ref_count-driven
/// COW forking. Consumes `tx` and commits internally on success.
async fn finalize_full_rewrite(
    mut tx: Box<dyn MetadataTx>,
    workspace_id: &str,
    norm_path: &str,
    old_file: &FileRecord,
    old_file_id: &str,
    meta: &WriteMeta,
) -> Result<api::WriteFileResponse> {
    if old_file.ref_count > 1 {
        let new_file_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let new_file = FileRecord {
            id: new_file_id.clone(),
            workspace_id: workspace_id.to_string(),
            size_bytes: meta.size,
            mime_type: old_file.mime_type.clone(),
            storage_type: meta.storage_type(),
            source_type: old_file.source_type,
            line_count: Some(meta.line_count),
            checksum_sha256: meta.sha256.clone(),
            revision: old_file.revision + 1,
            ref_count: 1,
            created_at: now,
            updated_at: now,
        };
        tx.insert_file(&new_file).await?;
        persist_write_meta(&mut *tx, &new_file_id, meta).await?;
        cleanup_file_if_orphan(&mut *tx, workspace_id, old_file_id).await?;
        tx.update_dentry_file_id(workspace_id, norm_path, &new_file_id)
            .await?;

        let outbox = make_outbox(workspace_id, OutboxEventType::ChunkSync, &new_file_id);
        tx.insert_outbox(&outbox).await?;
        let evt = make_fs_event(
            workspace_id,
            FsEventType::Update,
            norm_path,
            Some(&new_file_id),
        );
        tx.insert_fs_event(&evt).await?;
        tx.commit().await?;
        return Ok(api::WriteFileResponse {
            file_id: new_file_id,
            revision: new_file.revision,
            content_unchanged: false,
        });
    }

    let new_rev = old_file.revision + 1;
    tx.delete_file_content(old_file_id).await?;
    tx.delete_file_chunks(old_file_id).await?;
    persist_write_meta(&mut *tx, old_file_id, meta).await?;
    tx.update_file_revision(
        old_file_id,
        old_file.revision,
        new_rev,
        meta.size,
        &meta.sha256,
        Some(meta.line_count),
        meta.storage_type(),
    )
    .await?;

    let outbox = make_outbox(workspace_id, OutboxEventType::ChunkSync, old_file_id);
    tx.insert_outbox(&outbox).await?;
    let evt = make_fs_event(
        workspace_id,
        FsEventType::Update,
        norm_path,
        Some(old_file_id),
    );
    tx.insert_fs_event(&evt).await?;
    tx.commit().await?;
    Ok(api::WriteFileResponse {
        file_id: old_file_id.to_string(),
        revision: new_rev,
        content_unchanged: false,
    })
}

async fn ensure_parents(
    store: &dyn MetadataStore,
    workspace_id: &str,
    full_path: &str,
) -> Result<()> {
    let parent = path::parent(full_path);
    if parent == "/" {
        return Ok(());
    }

    let existing = store.get_dentry(workspace_id, parent).await?;
    if let Some(d) = existing {
        if !d.is_dir {
            return Err(VedaError::InvalidPath(format!(
                "{parent} exists as a file"
            )));
        }
        return Ok(());
    }

    Box::pin(ensure_parents(store, workspace_id, parent)).await?;

    let now = Utc::now();
    let dentry = Dentry {
        id: Uuid::new_v4().to_string(),
        workspace_id: workspace_id.to_string(),
        parent_path: path::parent(parent).to_string(),
        name: path::filename(parent).to_string(),
        path: parent.to_string(),
        file_id: None,
        is_dir: true,
        created_at: now,
        updated_at: now,
    };
    store.insert_dentry_ignore(&dentry).await
}

fn make_outbox(workspace_id: &str, event_type: OutboxEventType, file_id: &str) -> OutboxEvent {
    let now = Utc::now();
    OutboxEvent {
        id: 0,
        workspace_id: workspace_id.to_string(),
        event_type,
        payload: serde_json::json!({"file_id": file_id}),
        status: OutboxStatus::Pending,
        retry_count: 0,
        max_retries: 5,
        available_at: now,
        lease_until: None,
        created_at: now,
    }
}

fn make_fs_event(
    workspace_id: &str,
    event_type: FsEventType,
    path: &str,
    file_id: Option<&str>,
) -> FsEvent {
    FsEvent {
        id: 0,
        workspace_id: workspace_id.to_string(),
        event_type,
        path: path.to_string(),
        file_id: file_id.map(|s| s.to_string()),
        created_at: Utc::now(),
    }
}

/// Extract the longest fixed (non-glob) directory prefix from a glob pattern.
/// e.g. `/logs/*.jsonl` → `/logs`, `/data/**/*.csv` → `/data`
fn glob_fixed_prefix(pattern: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in pattern.split('/') {
        if part.contains('*') || part.contains('?') || part.contains('[') {
            break;
        }
        parts.push(part);
    }
    let joined = parts.join("/");
    if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    }
}

/// Simple glob matching: `*` matches any chars except `/`, `**` matches anything including `/`,
/// `?` matches a single char.
fn glob_match(pattern: &str, path: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_match_inner(pat: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_pi, mut star_si) = (usize::MAX, 0);
    let (mut dstar_pi, mut dstar_si) = (usize::MAX, 0);

    while si < s.len() {
        if pi + 1 < pat.len() && pat[pi] == b'*' && pat[pi + 1] == b'*' {
            dstar_pi = pi;
            dstar_si = si;
            pi += 2;
            if pi < pat.len() && pat[pi] == b'/' {
                pi += 1;
            }
            continue;
        }
        if pi < pat.len() && pat[pi] == b'*' {
            star_pi = pi;
            star_si = si;
            pi += 1;
            continue;
        }
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
            continue;
        }
        if star_pi != usize::MAX && s[star_si] != b'/' {
            star_si += 1;
            si = star_si;
            pi = star_pi + 1;
            continue;
        }
        if dstar_pi != usize::MAX {
            dstar_si += 1;
            si = dstar_si;
            pi = dstar_pi + 2;
            if pi < pat.len() && pat[pi] == b'/' {
                pi += 1;
            }
            continue;
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod append_hash_tests {
    use super::*;

    fn full_sha256(content: &str) -> String {
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        format!("{:x}", h.finalize())
    }

    #[test]
    fn append_meta_produces_true_content_sha256() {
        // Simulate a chunked file: two full chunks + a tail chunk.
        let chunk0 = "aaaaaaaaaa"; // 10 bytes
        let chunk1 = "bbbbbbbbbb"; // 10 bytes, last chunk (before append)
        let appended = "cc\n";
        let last_chunk_idx = 1;

        // Merged tail = old last chunk content + appended content
        let tail = format!("{chunk1}{appended}");
        let pre_tail: Vec<String> = vec![chunk0.to_string()];

        let meta = compute_append_meta_blocking(
            &pre_tail,
            0, // before_line_count
            last_chunk_idx,
            1, // tail_start_line
            tail.clone(),
            256 * 1024,
        );

        let reconstructed = format!("{chunk0}{chunk1}{appended}");
        assert_eq!(
            meta.sha256,
            full_sha256(&reconstructed),
            "append checksum must equal SHA-256 of full reconstructed content"
        );
    }

    #[test]
    fn append_meta_with_no_pre_tail_chunks() {
        // Single-chunk file being appended: pre_tail is empty.
        let tail = "hello world\n".to_string();
        let meta = compute_append_meta_blocking(&[], 0, 0, 1, tail.clone(), 256 * 1024);
        assert_eq!(meta.sha256, full_sha256(&tail));
    }
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn test_glob_fixed_prefix() {
        assert_eq!(glob_fixed_prefix("/logs/*.jsonl"), "/logs");
        assert_eq!(glob_fixed_prefix("/data/**/*.csv"), "/data");
        assert_eq!(glob_fixed_prefix("/*.txt"), "/");
        assert_eq!(glob_fixed_prefix("/a/b/c.txt"), "/a/b/c.txt");
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("/logs/*.jsonl", "/logs/app.jsonl"));
        assert!(!glob_match("/logs/*.jsonl", "/logs/sub/app.jsonl"));
        assert!(glob_match("/data/**/*.csv", "/data/a/b/c.csv"));
        assert!(glob_match("/data/**/*.csv", "/data/test.csv"));
        assert!(!glob_match("/data/**/*.csv", "/data/test.tsv"));
        assert!(glob_match("/?.txt", "/a.txt"));
        assert!(!glob_match("/?.txt", "/ab.txt"));
    }
}

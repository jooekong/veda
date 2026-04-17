use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;
use veda_types::*;

use crate::checksum::sha256_hex;
use crate::path;
use crate::store::MetadataStore;

const INLINE_THRESHOLD: i64 = 256 * 1024;
const CHUNK_SIZE: usize = 256 * 1024;

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
    ) -> Result<api::WriteFileResponse> {
        let norm = path::normalize(raw_path)?;
        let checksum = sha256_hex(content.as_bytes());
        let size = content.len() as i64;
        let line_count = Some(content.lines().count() as i32);

        let mut tx = self.meta.begin_tx().await?;
        let existing = tx.get_dentry(workspace_id, &norm).await?;

        if let Some(ref dentry) = existing {
            if dentry.is_dir {
                return Err(VedaError::AlreadyExists(format!(
                    "{norm} is a directory"
                )));
            }
            if let Some(ref fid) = dentry.file_id {
                let file = tx.get_file(fid).await?;
                if let Some(f) = file {
                    if f.checksum_sha256 == checksum {
                        tx.commit().await?;
                        return Ok(api::WriteFileResponse {
                            file_id: fid.clone(),
                            revision: f.revision,
                            content_unchanged: true,
                        });
                    }

                    if f.ref_count > 1 {
                        // COW: fork a new file record instead of mutating the shared one
                        let new_file_id = Uuid::new_v4().to_string();
                        let new_storage_type = if size <= INLINE_THRESHOLD {
                            StorageType::Inline
                        } else {
                            StorageType::Chunked
                        };
                        let now = Utc::now();
                        let new_file = FileRecord {
                            id: new_file_id.clone(),
                            workspace_id: workspace_id.to_string(),
                            size_bytes: size,
                            mime_type: f.mime_type.clone(),
                            storage_type: new_storage_type,
                            source_type: f.source_type,
                            line_count,
                            checksum_sha256: checksum.clone(),
                            revision: f.revision + 1,
                            ref_count: 1,
                            created_at: now,
                            updated_at: now,
                        };
                        tx.insert_file(&new_file).await?;
                        write_content(&mut *tx, &new_file_id, content, size).await?;
                        tx.decrement_ref_count(fid).await?;
                        tx.update_dentry_file_id(workspace_id, &norm, &new_file_id)
                            .await?;

                        let outbox =
                            make_outbox(workspace_id, OutboxEventType::ChunkSync, &new_file_id);
                        tx.insert_outbox(&outbox).await?;
                        let evt = make_fs_event(
                            workspace_id,
                            FsEventType::Update,
                            &norm,
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

                    let new_rev = f.revision + 1;
                    let new_storage_type = if size <= INLINE_THRESHOLD {
                        StorageType::Inline
                    } else {
                        StorageType::Chunked
                    };
                    tx.delete_file_content(fid).await?;
                    tx.delete_file_chunks(fid).await?;
                    write_content(&mut *tx, fid, content, size).await?;
                    tx.update_file_revision(
                        fid, new_rev, size, &checksum, line_count, new_storage_type,
                    )
                    .await?;

                    let outbox = make_outbox(workspace_id, OutboxEventType::ChunkSync, fid);
                    tx.insert_outbox(&outbox).await?;

                    let evt = make_fs_event(workspace_id, FsEventType::Update, &norm, Some(fid));
                    tx.insert_fs_event(&evt).await?;

                    tx.commit().await?;
                    return Ok(api::WriteFileResponse {
                        file_id: fid.clone(),
                        revision: new_rev,
                        content_unchanged: false,
                    });
                }
            }
        }

        // new file
        let file_id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let storage_type = if size <= INLINE_THRESHOLD {
            StorageType::Inline
        } else {
            StorageType::Chunked
        };

        let file = FileRecord {
            id: file_id.clone(),
            workspace_id: workspace_id.to_string(),
            size_bytes: size,
            mime_type: "text/plain".to_string(),
            storage_type,
            source_type: SourceType::Text,
            line_count,
            checksum_sha256: checksum.clone(),
            revision: 1,
            ref_count: 1,
            created_at: now,
            updated_at: now,
        };
        tx.insert_file(&file).await?;
        write_content(&mut *tx, &file_id, content, size).await?;

        if existing.is_some() {
            tx.update_dentry_file_id(workspace_id, &norm, &file_id)
                .await?;
        } else {
            ensure_parents(&mut *tx, workspace_id, &norm).await?;
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

    pub async fn read_file(&self, workspace_id: &str, raw_path: &str) -> Result<String> {
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
            .as_deref()
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;

        let file = self
            .meta
            .get_file(file_id)
            .await?
            .ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;

        match file.storage_type {
            StorageType::Inline => self
                .meta
                .get_file_content(file_id)
                .await?
                .ok_or_else(|| VedaError::NotFound(format!("content for {file_id}"))),
            StorageType::Chunked => {
                let chunks = self.meta.get_file_chunks(file_id, None, None).await?;
                Ok(chunks.into_iter().map(|c| c.content).collect::<String>())
            }
        }
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
            .as_deref()
            .ok_or_else(|| VedaError::NotFound(norm.clone()))?;
        let file = self
            .meta
            .get_file(file_id)
            .await?
            .ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;

        let content = match file.storage_type {
            StorageType::Inline => self
                .meta
                .get_file_content(file_id)
                .await?
                .ok_or_else(|| VedaError::NotFound(format!("content for {file_id}")))?,
            StorageType::Chunked => {
                let chunks = self
                    .meta
                    .get_file_chunks(file_id, None, Some(end))
                    .await?;
                chunks.into_iter().map(|c| c.content).collect::<String>()
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let s = (start - 1) as usize;
        let e = (end as usize).min(lines.len());
        if s >= lines.len() {
            return Ok(String::new());
        }
        Ok(lines[s..e].join("\n"))
    }

    pub async fn list_dir(
        &self,
        workspace_id: &str,
        raw_path: &str,
    ) -> Result<Vec<api::DirEntry>> {
        let norm = path::normalize(raw_path)?;
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
        let mut all = Vec::new();
        let top = self.meta.list_dentries(workspace_id, &norm).await?;
        let mut queue: Vec<String> = Vec::new();
        for d in top {
            if d.is_dir {
                queue.push(d.path.clone());
            }
            all.push(d);
            if all.len() > max_entries {
                return Err(VedaError::QuotaExceeded(
                    format!("directory listing exceeded {} entries", max_entries),
                ));
            }
        }
        while let Some(dir) = queue.pop() {
            let children = self.meta.list_dentries(workspace_id, &dir).await?;
            for c in children {
                if c.is_dir {
                    queue.push(c.path.clone());
                }
                all.push(c);
                if all.len() > max_entries {
                    return Err(VedaError::QuotaExceeded(
                        format!("directory listing exceeded {} entries", max_entries),
                    ));
                }
            }
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
        let all = self.list_dir_recursive(workspace_id, &prefix, max_matches).await?;
        Ok(all
            .into_iter()
            .filter(|d| !d.is_dir && glob_match(pattern, &d.path))
            .collect())
    }

    pub async fn stat(
        &self,
        workspace_id: &str,
        raw_path: &str,
    ) -> Result<api::FileInfo> {
        let norm = path::normalize(raw_path)?;
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
    pub async fn delete(
        &self,
        workspace_id: &str,
        raw_path: &str,
    ) -> Result<u64> {
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

        if dentry.is_dir {
            let children = tx.list_dentries_under(workspace_id, &norm).await?;
            deleted_count += children.len() as u64;
            for child in &children {
                if let Some(ref fid) = child.file_id {
                    file_ids_to_cleanup.push(fid.clone());
                }
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
        tx.commit().await?;
        Ok(deleted_count)
    }

    pub async fn mkdir(&self, workspace_id: &str, raw_path: &str) -> Result<()> {
        let norm = path::normalize(raw_path)?;
        if norm == "/" {
            return Ok(());
        }

        let mut tx = self.meta.begin_tx().await?;

        let existing = tx.get_dentry(workspace_id, &norm).await?;
        if let Some(d) = existing {
            tx.commit().await?;
            if d.is_dir {
                return Ok(());
            }
            return Err(VedaError::AlreadyExists(format!(
                "{norm} exists as a file"
            )));
        }

        ensure_parents(&mut *tx, workspace_id, &norm).await?;

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
        match tx.insert_dentry(&dentry).await {
            Ok(()) => {
                tx.commit().await?;
                Ok(())
            }
            Err(VedaError::Storage(msg)) if msg.contains("Duplicate entry") => {
                tx.rollback().await.ok();
                Ok(())
            }
            Err(e) => Err(e),
        }
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

        let src_dentry = self
            .meta
            .get_dentry(workspace_id, &src)
            .await?
            .ok_or_else(|| VedaError::NotFound(src.clone()))?;

        if src_dentry.is_dir {
            return Err(VedaError::InvalidInput(
                "cannot copy a directory".to_string(),
            ));
        }

        let file_id = src_dentry
            .file_id
            .as_deref()
            .ok_or_else(|| VedaError::NotFound(src.clone()))?;

        let mut tx = self.meta.begin_tx().await?;

        let existing_dst = tx.get_dentry(workspace_id, &dst).await?;
        if let Some(ref d) = existing_dst {
            if d.is_dir {
                tx.rollback().await?;
                return Err(VedaError::AlreadyExists(format!(
                    "{dst} is a directory"
                )));
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
        ensure_parents(&mut *tx, workspace_id, &dst).await?;

        if let Some(d) = existing_dst {
            if let Some(ref old_fid) = d.file_id {
                let remaining = tx.decrement_ref_count(old_fid).await?;
                if remaining <= 0 {
                    tx.delete_file_content(old_fid).await?;
                    tx.delete_file_chunks(old_fid).await?;
                    let outbox =
                        make_outbox(workspace_id, OutboxEventType::ChunkDelete, old_fid);
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
        let norm = path::normalize(raw_path)?;

        let existing = self.meta.get_dentry(workspace_id, &norm).await?;
        let old_content = match existing {
            Some(ref d) if d.is_dir => {
                return Err(VedaError::AlreadyExists(format!("{norm} is a directory")));
            }
            Some(ref d) if d.file_id.is_some() => {
                Some(self.read_file(workspace_id, &norm).await?)
            }
            _ => None,
        };

        let new_content = match old_content {
            Some(mut old) => {
                old.push_str(content);
                old
            }
            None => content.to_string(),
        };

        self.write_file(workspace_id, &norm, &new_content).await
    }

    pub async fn rename(
        &self,
        workspace_id: &str,
        src_path: &str,
        dst_path: &str,
    ) -> Result<()> {
        let src = path::normalize(src_path)?;
        let dst = path::normalize(dst_path)?;

        if src == dst {
            return Ok(());
        }

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

        let children = if src_dentry.is_dir {
            tx.list_dentries_under(workspace_id, &src).await?
        } else {
            Vec::new()
        };

        ensure_parents(&mut *tx, workspace_id, &dst).await?;

        tx.rename_dentry(
            workspace_id,
            &src,
            &dst,
            path::parent(&dst),
            path::filename(&dst),
        )
        .await?;

        for child in &children {
            let new_child_path = format!("{dst}{}", &child.path[src.len()..]);
            let new_parent = path::parent(&new_child_path).to_string();
            tx.rename_dentry(
                workspace_id,
                &child.path,
                &new_child_path,
                &new_parent,
                &child.name,
            )
            .await?;
        }

        let evt = make_fs_event(
            workspace_id,
            FsEventType::Move,
            &dst,
            src_dentry.file_id.as_deref(),
        );
        tx.insert_fs_event(&evt).await?;
        tx.commit().await?;
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────

async fn write_content(
    tx: &mut dyn crate::store::MetadataTx,
    file_id: &str,
    content: &str,
    size: i64,
) -> Result<()> {
    if size <= INLINE_THRESHOLD {
        tx.insert_file_content(file_id, content).await
    } else {
        let chunks = split_into_chunks(file_id, content);
        tx.insert_file_chunks(&chunks).await
    }
}

fn split_into_chunks(file_id: &str, content: &str) -> Vec<FileChunk> {
    let bytes = content.as_bytes();
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut chunk_index = 0;
    let mut current_line = 1i32;

    while offset < bytes.len() {
        let mut end = (offset + CHUNK_SIZE).min(bytes.len());
        if end < bytes.len() {
            if let Some(nl) = bytes[offset..end].iter().rposition(|&b| b == b'\n') {
                end = offset + nl + 1;
            }
        }

        let chunk_content = String::from_utf8_lossy(&bytes[offset..end]).to_string();
        let line_count = chunk_content.matches('\n').count() as i32;

        chunks.push(FileChunk {
            file_id: file_id.to_string(),
            chunk_index,
            start_line: current_line,
            content: chunk_content,
        });

        current_line += line_count;
        chunk_index += 1;
        offset = end;
    }

    chunks
}

async fn ensure_parents(
    tx: &mut dyn crate::store::MetadataTx,
    workspace_id: &str,
    full_path: &str,
) -> Result<()> {
    let parent = path::parent(full_path);
    if parent == "/" {
        return Ok(());
    }

    let existing = tx.get_dentry(workspace_id, parent).await?;
    if existing.is_some() {
        return Ok(());
    }

    // recursively create parents
    Box::pin(ensure_parents(tx, workspace_id, parent)).await?;

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
    tx.insert_dentry(&dentry).await?;
    Ok(())
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

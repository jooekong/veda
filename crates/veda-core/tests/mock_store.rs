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
    pub file_chunks: Vec<FileChunk>,
    pub outbox: Vec<OutboxEvent>,
    pub fs_events: Vec<FsEvent>,
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
fn filter_overlap(
    mut chunks: Vec<FileChunk>,
    start_line: Option<i32>,
    end_line: Option<i32>,
) -> Vec<FileChunk> {
    if let Some(b) = end_line {
        chunks.retain(|c| c.start_line <= b);
    }
    if let Some(a) = start_line {
        // keep the largest chunk_index among those with start_line <= a, and all after it
        let base_idx = chunks
            .iter()
            .filter(|c| c.start_line <= a)
            .map(|c| c.chunk_index)
            .max()
            .unwrap_or(0);
        chunks.retain(|c| c.chunk_index >= base_idx);
    }
    chunks
}

#[async_trait]
impl MetadataStore for MockMetadataStore {
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

    async fn list_dentries_under(
        &self,
        workspace_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        if path_prefix == "/" {
            return Ok(st
                .dentries
                .iter()
                .filter(|d| d.workspace_id == workspace_id)
                .cloned()
                .collect());
        }
        let prefix = format!("{path_prefix}/");
        Ok(st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && d.path.starts_with(&prefix))
            .cloned()
            .collect())
    }

    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>> {
        let st = self.state.lock().unwrap();
        Ok(st.files.iter().find(|f| f.id == file_id).cloned())
    }

    async fn get_file_content(&self, file_id: &str) -> Result<Option<String>> {
        let st = self.state.lock().unwrap();
        Ok(st.file_contents.get(file_id).cloned())
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

    async fn find_file_by_checksum(
        &self,
        workspace_id: &str,
        checksum: &str,
    ) -> Result<Option<FileRecord>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .files
            .iter()
            .find(|f| f.workspace_id == workspace_id && f.checksum_sha256 == checksum)
            .cloned())
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

    async fn query_fs_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FsEvent>> {
        let st = self.state.lock().unwrap();
        let mut events: Vec<FsEvent> = st
            .fs_events
            .iter()
            .filter(|e| {
                e.workspace_id == workspace_id
                    && e.id > since_id
                    && path_prefix.map(|p| e.path.starts_with(p)).unwrap_or(true)
            })
            .cloned()
            .collect();
        events.sort_by_key(|e| e.id);
        events.truncate(limit);
        Ok(events)
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

    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> {
        Ok(Box::new(MockTx {
            state: Arc::clone(&self.state),
        }))
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

    async fn list_dentries_under(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .filter(|d| {
                d.workspace_id == workspace_id && d.path.starts_with(&format!("{path_prefix}/"))
            })
            .cloned()
            .collect())
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

    async fn get_last_file_chunk(&mut self, file_id: &str) -> Result<Option<FileChunk>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file_id)
            .max_by_key(|c| c.chunk_index)
            .cloned())
    }

    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.outbox.push(event.clone());
        Ok(())
    }

    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.fs_events.push(event.clone());
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use veda_types::*;
use veda_core::store::*;

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

    async fn list_dentries(
        &self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<Vec<Dentry>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .dentries
            .iter()
            .filter(|d| d.workspace_id == workspace_id && d.parent_path == parent_path)
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
        _start_line: Option<i32>,
        _end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let st = self.state.lock().unwrap();
        let mut chunks: Vec<FileChunk> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file_id)
            .cloned()
            .collect();
        chunks.sort_by_key(|c| c.chunk_index);
        Ok(chunks)
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
        revision: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
    ) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = st.files.iter_mut().find(|f| f.id == file_id) {
            f.revision = revision;
            f.size_bytes = size_bytes;
            f.checksum_sha256 = checksum.to_string();
            f.line_count = line_count;
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

// ── Mock TaskQueue ─────────────────────────────────────

pub struct MockTaskQueue {
    pub state: Arc<Mutex<MockState>>,
}

impl MockTaskQueue {
    pub fn new(state: Arc<Mutex<MockState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl TaskQueue for MockTaskQueue {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.outbox.push(event.clone());
        Ok(())
    }

    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>> {
        let st = self.state.lock().unwrap();
        Ok(st
            .outbox
            .iter()
            .filter(|e| e.status == OutboxStatus::Pending)
            .take(batch_size)
            .cloned()
            .collect())
    }

    async fn complete(&self, _task_id: i64) -> Result<()> {
        Ok(())
    }

    async fn fail(&self, _task_id: i64, _error: &str) -> Result<()> {
        Ok(())
    }
}

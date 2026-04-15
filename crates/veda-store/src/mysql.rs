//! MySQL-backed metadata store, transactional metadata, and outbox task queue.

use async_trait::async_trait;
use sqlx::{MySqlPool, Row, Transaction};
use sqlx::types::Json;
use veda_core::store::{MetadataStore, MetadataTx, TaskQueue};
use veda_types::{
    Dentry, FileChunk, FileRecord, FsEvent, FsEventType, OutboxEvent, OutboxEventType,
    OutboxStatus, Result, SourceType, StorageType, VedaError,
};

fn storage_err(e: impl ToString) -> VedaError {
    VedaError::Storage(e.to_string())
}

fn parse_storage_type(s: &str) -> Result<StorageType> {
    match s {
        "inline" => Ok(StorageType::Inline),
        "chunked" => Ok(StorageType::Chunked),
        _ => Err(storage_err(format!("unknown storage_type: {s}"))),
    }
}

fn storage_type_str(s: StorageType) -> &'static str {
    match s {
        StorageType::Inline => "inline",
        StorageType::Chunked => "chunked",
    }
}

fn parse_source_type(s: &str) -> Result<SourceType> {
    match s {
        "text" => Ok(SourceType::Text),
        "pdf" => Ok(SourceType::Pdf),
        "image" => Ok(SourceType::Image),
        _ => Err(storage_err(format!("unknown source_type: {s}"))),
    }
}

fn source_type_str(s: SourceType) -> &'static str {
    match s {
        SourceType::Text => "text",
        SourceType::Pdf => "pdf",
        SourceType::Image => "image",
    }
}

fn parse_outbox_event_type(s: &str) -> Result<OutboxEventType> {
    match s {
        "chunk_sync" => Ok(OutboxEventType::ChunkSync),
        "chunk_delete" => Ok(OutboxEventType::ChunkDelete),
        "collection_sync" => Ok(OutboxEventType::CollectionSync),
        _ => Err(storage_err(format!("unknown outbox event_type: {s}"))),
    }
}

fn outbox_event_type_str(t: OutboxEventType) -> &'static str {
    match t {
        OutboxEventType::ChunkSync => "chunk_sync",
        OutboxEventType::ChunkDelete => "chunk_delete",
        OutboxEventType::CollectionSync => "collection_sync",
    }
}

fn parse_outbox_status(s: &str) -> Result<OutboxStatus> {
    match s {
        "pending" => Ok(OutboxStatus::Pending),
        "processing" => Ok(OutboxStatus::Processing),
        "completed" => Ok(OutboxStatus::Completed),
        "failed" => Ok(OutboxStatus::Failed),
        "dead" => Ok(OutboxStatus::Dead),
        _ => Err(storage_err(format!("unknown outbox status: {s}"))),
    }
}

fn outbox_status_str(s: OutboxStatus) -> &'static str {
    match s {
        OutboxStatus::Pending => "pending",
        OutboxStatus::Processing => "processing",
        OutboxStatus::Completed => "completed",
        OutboxStatus::Failed => "failed",
        OutboxStatus::Dead => "dead",
    }
}

fn fs_event_type_str(t: FsEventType) -> &'static str {
    match t {
        FsEventType::Create => "create",
        FsEventType::Update => "update",
        FsEventType::Delete => "delete",
        FsEventType::Move => "move",
    }
}

fn row_to_dentry(row: &sqlx::mysql::MySqlRow) -> Result<Dentry> {
    let file_id: Option<String> = row.try_get("file_id").map_err(storage_err)?;
    Ok(Dentry {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        parent_path: row.try_get("parent_path").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        path: row.try_get("path").map_err(storage_err)?,
        file_id,
        is_dir: row.try_get::<bool, _>("is_dir").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_file(row: &sqlx::mysql::MySqlRow) -> Result<FileRecord> {
    let st: String = row.try_get("storage_type").map_err(storage_err)?;
    let src: String = row.try_get("source_type").map_err(storage_err)?;
    Ok(FileRecord {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        size_bytes: row.try_get("size_bytes").map_err(storage_err)?,
        mime_type: row.try_get("mime_type").map_err(storage_err)?,
        storage_type: parse_storage_type(&st)?,
        source_type: parse_source_type(&src)?,
        line_count: row.try_get("line_count").map_err(storage_err)?,
        checksum_sha256: row.try_get("checksum_sha256").map_err(storage_err)?,
        revision: row.try_get("revision").map_err(storage_err)?,
        ref_count: row.try_get("ref_count").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_file_chunk(row: &sqlx::mysql::MySqlRow) -> Result<FileChunk> {
    Ok(FileChunk {
        file_id: row.try_get("file_id").map_err(storage_err)?,
        chunk_index: row.try_get("chunk_index").map_err(storage_err)?,
        start_line: row.try_get("start_line").map_err(storage_err)?,
        content: row.try_get("content").map_err(storage_err)?,
    })
}

fn row_to_outbox(row: &sqlx::mysql::MySqlRow) -> Result<OutboxEvent> {
    let et: String = row.try_get("event_type").map_err(storage_err)?;
    let st: String = row.try_get("status").map_err(storage_err)?;
    let Json(payload): Json<serde_json::Value> = row.try_get("payload").map_err(storage_err)?;
    Ok(OutboxEvent {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        event_type: parse_outbox_event_type(&et)?,
        payload,
        status: parse_outbox_status(&st)?,
        retry_count: row.try_get("retry_count").map_err(storage_err)?,
        max_retries: row.try_get("max_retries").map_err(storage_err)?,
        available_at: row.try_get("available_at").map_err(storage_err)?,
        lease_until: row.try_get("lease_until").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
    })
}

pub struct MysqlStore {
    pool: MySqlPool,
}

impl MysqlStore {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = MySqlPool::connect(database_url)
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        let stmts = [
            r#"CREATE TABLE IF NOT EXISTS veda_dentries (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    parent_path VARCHAR(4096) NOT NULL,
    name VARCHAR(255) NOT NULL,
    path VARCHAR(4096) NOT NULL,
    file_id VARCHAR(36),
    is_dir BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_ws_path (workspace_id, path(255)),
    INDEX idx_parent (workspace_id, parent_path(255))
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_files (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    mime_type VARCHAR(128) DEFAULT 'text/plain',
    storage_type VARCHAR(16) NOT NULL DEFAULT 'inline',
    source_type VARCHAR(16) NOT NULL DEFAULT 'text',
    line_count INT,
    checksum_sha256 VARCHAR(64) NOT NULL,
    revision INT NOT NULL DEFAULT 1,
    ref_count INT NOT NULL DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    INDEX idx_workspace (workspace_id),
    INDEX idx_checksum (workspace_id, checksum_sha256)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_file_contents (
    file_id VARCHAR(36) PRIMARY KEY,
    content LONGTEXT NOT NULL
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_file_chunks (
    file_id VARCHAR(36) NOT NULL,
    chunk_index INT NOT NULL,
    start_line INT NOT NULL,
    content MEDIUMTEXT NOT NULL,
    PRIMARY KEY (file_id, chunk_index),
    INDEX idx_line_lookup (file_id, start_line)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_outbox (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    event_type VARCHAR(32) NOT NULL,
    payload JSON NOT NULL,
    status VARCHAR(16) DEFAULT 'pending',
    retry_count INT NOT NULL DEFAULT 0,
    max_retries INT NOT NULL DEFAULT 5,
    available_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    lease_until TIMESTAMP NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_claim (status, available_at)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_fs_events (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    event_type VARCHAR(16) NOT NULL,
    path VARCHAR(4096) NOT NULL,
    file_id VARCHAR(36),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ws_poll (workspace_id, id)
)"#,
        ];
        for s in stmts {
            sqlx::query(s)
                .execute(&self.pool)
                .await
                .map_err(|e| VedaError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }
}

async fn get_dentry_conn(
    conn: &mut sqlx::MySqlConnection,
    workspace_id: &str,
    path: &str,
) -> Result<Option<Dentry>> {
    let row = sqlx::query(
        r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
           FROM veda_dentries WHERE workspace_id = ? AND path = ?"#,
    )
    .bind(workspace_id)
    .bind(path)
    .fetch_optional(conn)
    .await
    .map_err(storage_err)?;
    row.map(|r| row_to_dentry(&r)).transpose()
}

async fn get_file_conn(
    conn: &mut sqlx::MySqlConnection,
    file_id: &str,
) -> Result<Option<FileRecord>> {
    let row = sqlx::query(
        r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
                  checksum_sha256, revision, ref_count, created_at, updated_at
           FROM veda_files WHERE id = ?"#,
    )
    .bind(file_id)
    .fetch_optional(conn)
    .await
    .map_err(storage_err)?;
    row.map(|r| row_to_file(&r)).transpose()
}

async fn insert_outbox_conn(conn: &mut sqlx::MySqlConnection, event: &OutboxEvent) -> Result<()> {
    let payload = serde_json::to_string(&event.payload).map_err(|e| storage_err(e.to_string()))?;
    let status = outbox_status_str(event.status);
    let et = outbox_event_type_str(event.event_type);
    if event.id == 0 {
        sqlx::query(
            r#"INSERT INTO veda_outbox
            (workspace_id, event_type, payload, status, retry_count, max_retries, available_at, lease_until, created_at)
            VALUES (?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&payload)
        .bind(status)
        .bind(event.retry_count)
        .bind(event.max_retries)
        .bind(event.available_at.naive_utc())
        .bind(event.lease_until.map(|x| x.naive_utc()))
        .bind(event.created_at.naive_utc())
        .execute(conn)
        .await
        .map_err(storage_err)?;
    } else {
        sqlx::query(
            r#"INSERT INTO veda_outbox
            (id, workspace_id, event_type, payload, status, retry_count, max_retries, available_at, lease_until, created_at)
            VALUES (?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(event.id)
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&payload)
        .bind(status)
        .bind(event.retry_count)
        .bind(event.max_retries)
        .bind(event.available_at.naive_utc())
        .bind(event.lease_until.map(|x| x.naive_utc()))
        .bind(event.created_at.naive_utc())
        .execute(conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for MysqlStore {
    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        get_dentry_conn(&mut *conn, workspace_id, path).await
    }

    async fn list_dentries(&self, workspace_id: &str, parent_path: &str) -> Result<Vec<Dentry>> {
        let mut rows = sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries WHERE workspace_id = ? AND parent_path = ? ORDER BY path"#,
        )
        .bind(workspace_id)
        .bind(parent_path)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows.drain(..) {
            out.push(row_to_dentry(&r)?);
        }
        Ok(out)
    }

    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        get_file_conn(&mut *conn, file_id).await
    }

    async fn get_file_content(&self, file_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(r#"SELECT content FROM veda_file_contents WHERE file_id = ?"#)
            .bind(file_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(row
            .map(|r| r.try_get::<String, _>("content"))
            .transpose()
            .map_err(storage_err)?)
    }

    async fn get_file_chunks(
        &self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let mut q = String::from(
            r#"SELECT file_id, chunk_index, start_line, content FROM veda_file_chunks WHERE file_id = ?"#,
        );
        let mut rows = match (start_line, end_line) {
            (Some(a), Some(b)) => {
                q.push_str(" AND start_line >= ? AND start_line <= ? ORDER BY chunk_index");
                sqlx::query(&q)
                    .bind(file_id)
                    .bind(a)
                    .bind(b)
                    .fetch_all(&self.pool)
                    .await
            }
            (Some(a), None) => {
                q.push_str(" AND start_line >= ? ORDER BY chunk_index");
                sqlx::query(&q).bind(file_id).bind(a).fetch_all(&self.pool).await
            }
            (None, Some(b)) => {
                q.push_str(" AND start_line <= ? ORDER BY chunk_index");
                sqlx::query(&q).bind(file_id).bind(b).fetch_all(&self.pool).await
            }
            (None, None) => {
                q.push_str(" ORDER BY chunk_index");
                sqlx::query(&q).bind(file_id).fetch_all(&self.pool).await
            }
        }
        .map_err(storage_err)?;
        let mut v = Vec::with_capacity(rows.len());
        for r in rows.drain(..) {
            v.push(row_to_file_chunk(&r)?);
        }
        Ok(v)
    }

    async fn find_file_by_checksum(
        &self,
        workspace_id: &str,
        checksum: &str,
    ) -> Result<Option<FileRecord>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
                      checksum_sha256, revision, ref_count, created_at, updated_at
               FROM veda_files WHERE workspace_id = ? AND checksum_sha256 = ? LIMIT 1"#,
        )
        .bind(workspace_id)
        .bind(checksum)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_file(&r)).transpose()
    }

    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(Box::new(MysqlMetadataTx { tx: Some(tx) }))
    }
}

pub struct MysqlMetadataTx {
    tx: Option<Transaction<'static, sqlx::MySql>>,
}

impl MysqlMetadataTx {
    fn tx_mut(&mut self) -> Result<&mut Transaction<'static, sqlx::MySql>> {
        self.tx
            .as_mut()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))
    }
}

#[async_trait]
impl MetadataTx for MysqlMetadataTx {
    async fn get_dentry(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let t = self.tx_mut()?;
        get_dentry_conn(t.as_mut(), workspace_id, path).await
    }

    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_dentries
            (id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&dentry.id)
        .bind(&dentry.workspace_id)
        .bind(&dentry.parent_path)
        .bind(&dentry.name)
        .bind(&dentry.path)
        .bind(&dentry.file_id)
        .bind(dentry.is_dir)
        .bind(dentry.created_at.naive_utc())
        .bind(dentry.updated_at.naive_utc())
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn update_dentry_file_id(
        &mut self,
        workspace_id: &str,
        path: &str,
        file_id: &str,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"UPDATE veda_dentries SET file_id = ? WHERE workspace_id = ? AND path = ?"#,
        )
        .bind(file_id)
        .bind(workspace_id)
        .bind(path)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_dentry(&mut self, workspace_id: &str, path: &str) -> Result<u64> {
        let t = self.tx_mut()?;
        let r = sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path = ?"#)
            .bind(workspace_id)
            .bind(path)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn list_dentries_under(&mut self, workspace_id: &str, path_prefix: &str) -> Result<Vec<Dentry>> {
        let t = self.tx_mut()?;
        let like = format!("{path_prefix}/%");
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries WHERE workspace_id = ? AND path LIKE ?"#,
        )
        .bind(workspace_id)
        .bind(&like)
        .fetch_all(t.as_mut())
        .await
        .map_err(storage_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_dentry(r)?);
        }
        Ok(out)
    }

    async fn delete_dentries_under(&mut self, workspace_id: &str, parent_path: &str) -> Result<u64> {
        let t = self.tx_mut()?;
        let r = if parent_path == "/" {
            sqlx::query(
                r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path <> '/' AND path LIKE '/%'"#,
            )
            .bind(workspace_id)
            .execute(t.as_mut())
            .await
        } else {
            let like = format!("{parent_path}/%");
            sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path LIKE ?"#)
                .bind(workspace_id)
                .bind(like)
                .execute(t.as_mut())
                .await
        }
        .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn rename_dentry(
        &mut self,
        workspace_id: &str,
        old_path: &str,
        new_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"UPDATE veda_dentries SET path = ?, parent_path = ?, name = ?
               WHERE workspace_id = ? AND path = ?"#,
        )
        .bind(new_path)
        .bind(new_parent)
        .bind(new_name)
        .bind(workspace_id)
        .bind(old_path)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>> {
        let t = self.tx_mut()?;
        get_file_conn(t.as_mut(), file_id).await
    }

    async fn insert_file(&mut self, file: &FileRecord) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_files
            (id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
             checksum_sha256, revision, ref_count, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&file.id)
        .bind(&file.workspace_id)
        .bind(file.size_bytes)
        .bind(&file.mime_type)
        .bind(storage_type_str(file.storage_type))
        .bind(source_type_str(file.source_type))
        .bind(file.line_count)
        .bind(&file.checksum_sha256)
        .bind(file.revision)
        .bind(file.ref_count)
        .bind(file.created_at.naive_utc())
        .bind(file.updated_at.naive_utc())
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn update_file_revision(
        &mut self,
        file_id: &str,
        revision: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"UPDATE veda_files SET revision = ?, size_bytes = ?, checksum_sha256 = ?, line_count = ?, storage_type = ?
               WHERE id = ?"#,
        )
        .bind(revision)
        .bind(size_bytes)
        .bind(checksum)
        .bind(line_count)
        .bind(storage_type_str(storage_type))
        .bind(file_id)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32> {
        let t = self.tx_mut()?;
        sqlx::query(r#"UPDATE veda_files SET ref_count = ref_count - 1 WHERE id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        let row = sqlx::query(r#"SELECT ref_count FROM veda_files WHERE id = ?"#)
            .bind(file_id)
            .fetch_optional(t.as_mut())
            .await
            .map_err(storage_err)?;
        let r = row.ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;
        Ok(r.try_get::<i32, _>("ref_count").map_err(storage_err)?)
    }

    async fn increment_ref_count(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"UPDATE veda_files SET ref_count = ref_count + 1 WHERE id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_files WHERE id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_file_contents (file_id, content) VALUES (?, ?)
               ON DUPLICATE KEY UPDATE content = VALUES(content)"#,
        )
        .bind(file_id)
        .bind(content)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_content(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_contents WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()> {
        let t = self.tx_mut()?;
        for c in chunks {
            sqlx::query(
                r#"INSERT INTO veda_file_chunks (file_id, chunk_index, start_line, content)
                   VALUES (?, ?, ?, ?)"#,
            )
            .bind(&c.file_id)
            .bind(c.chunk_index)
            .bind(c.start_line)
            .bind(&c.content)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        }
        Ok(())
    }

    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_chunks WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()> {
        let t = self.tx_mut()?;
        insert_outbox_conn(t.as_mut(), event).await
    }

    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> {
        let t = self.tx_mut()?;
        let et = fs_event_type_str(event.event_type);
        if event.id == 0 {
            sqlx::query(
                r#"INSERT INTO veda_fs_events (workspace_id, event_type, path, file_id, created_at)
                   VALUES (?, ?, ?, ?, ?)"#,
            )
            .bind(&event.workspace_id)
            .bind(et)
            .bind(&event.path)
            .bind(&event.file_id)
            .bind(event.created_at.naive_utc())
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        } else {
            sqlx::query(
                r#"INSERT INTO veda_fs_events (id, workspace_id, event_type, path, file_id, created_at)
                   VALUES (?, ?, ?, ?, ?, ?)"#,
            )
            .bind(event.id)
            .bind(&event.workspace_id)
            .bind(et)
            .bind(&event.path)
            .bind(&event.file_id)
            .bind(event.created_at.naive_utc())
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        let tx = self
            .tx
            .take()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))?;
        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        let tx = self
            .tx
            .take()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))?;
        tx.rollback().await.map_err(storage_err)?;
        Ok(())
    }
}

#[async_trait]
impl TaskQueue for MysqlStore {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        insert_outbox_conn(&mut *conn, event).await
    }

    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>> {
        let batch_size_i64 = i64::try_from(batch_size).unwrap_or(100);
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, event_type, payload, status, retry_count, max_retries,
                      available_at, lease_until, created_at
               FROM veda_outbox
               WHERE status = 'pending' AND available_at <= UTC_TIMESTAMP()
               ORDER BY id ASC
               LIMIT ?
               FOR UPDATE SKIP LOCKED"#,
        )
        .bind(batch_size_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_err)?;
        let mut ids = Vec::new();
        let mut events = Vec::new();
        for r in &rows {
            let id: i64 = r.try_get("id").map_err(storage_err)?;
            ids.push(id);
            events.push(row_to_outbox(r)?);
        }
        for id in &ids {
            sqlx::query(
                r#"UPDATE veda_outbox SET status = 'processing',
                   lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL 10 MINUTE)
                   WHERE id = ?"#,
            )
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(storage_err)?;
        }
        tx.commit().await.map_err(storage_err)?;
        Ok(events)
    }

    async fn complete(&self, task_id: i64) -> Result<()> {
        sqlx::query(r#"UPDATE veda_outbox SET status = 'completed', lease_until = NULL WHERE id = ?"#)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn fail(&self, task_id: i64, error: &str) -> Result<()> {
        let row = sqlx::query(
            r#"SELECT id, retry_count, max_retries, payload FROM veda_outbox WHERE id = ?"#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        let Some(r) = row else {
            return Ok(());
        };
        let retry: i32 = r.try_get("retry_count").map_err(storage_err)?;
        let max: i32 = r.try_get("max_retries").map_err(storage_err)?;
        let Json(mut payload): Json<serde_json::Value> = r.try_get("payload").map_err(storage_err)?;
        if let serde_json::Value::Object(ref mut m) = payload {
            m.insert(
                "_last_error".into(),
                serde_json::Value::String(error.to_string()),
            );
        }
        let payload_str = serde_json::to_string(&payload).map_err(|e| storage_err(e.to_string()))?;
        let next_retry = retry + 1;
        if next_retry >= max {
            sqlx::query(
                r#"UPDATE veda_outbox SET status = 'dead', retry_count = ?, payload = CAST(? AS JSON),
                   lease_until = NULL WHERE id = ?"#,
            )
            .bind(next_retry)
            .bind(&payload_str)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        } else {
            sqlx::query(
                r#"UPDATE veda_outbox SET status = 'pending', retry_count = ?, payload = CAST(? AS JSON),
                   available_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL 30 SECOND), lease_until = NULL WHERE id = ?"#,
            )
            .bind(next_retry)
            .bind(&payload_str)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        }
        Ok(())
    }
}

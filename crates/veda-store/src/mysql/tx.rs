use super::*;

pub struct MysqlMetadataTx {
    pub(super) tx: Option<Transaction<'static, sqlx::MySql>>,
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
        let row = sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries WHERE workspace_id = ? AND path = ? FOR UPDATE"#,
        )
        .bind(workspace_id)
        .bind(path)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_dentry(&r)).transpose()
    }

    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()> {
        let t = self.tx_mut()?;
        match sqlx::query(
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
        {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(ref db_err))
                if db_err
                    .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                    .is_some_and(|e| e.number() == 1062) =>
            {
                Err(VedaError::AlreadyExists(format!("dentry {}", dentry.path)))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn update_dentry_file_id(
        &mut self,
        workspace_id: &str,
        path: &str,
        file_id: &str,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"UPDATE veda_dentries SET file_id = ? WHERE workspace_id = ? AND path = ?"#)
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

    async fn list_dentries_under_page(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let t = self.tx_mut()?;
        list_dentries_under_page_conn(t.as_mut(), workspace_id, path_prefix, after_path, limit)
            .await
    }

    async fn delete_dentries_under(
        &mut self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<u64> {
        let t = self.tx_mut()?;
        let r = if parent_path == "/" {
            sqlx::query(
                r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path <> '/' AND path LIKE '/%'"#,
            )
            .bind(workspace_id)
            .execute(t.as_mut())
            .await
        } else {
            let like = format!("{}/%", escape_like(parent_path));
            sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\'"#)
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

    async fn rename_dentries_under(
        &mut self,
        workspace_id: &str,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<u64> {
        let t = self.tx_mut()?;
        // CHAR_LENGTH (not Rust's byte len) — MySQL SUBSTRING on VARCHAR uses
        // character offsets, so Unicode paths need character-based slicing.
        let like = format!("{}/%", escape_like(old_prefix));
        let r = sqlx::query(
            r#"UPDATE veda_dentries
               SET path = CONCAT(?, SUBSTRING(path, CHAR_LENGTH(?) + 1)),
                   parent_path = CONCAT(?, SUBSTRING(parent_path, CHAR_LENGTH(?) + 1))
               WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\'"#,
        )
        .bind(new_prefix)
        .bind(old_prefix)
        .bind(new_prefix)
        .bind(old_prefix)
        .bind(workspace_id)
        .bind(&like)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>> {
        let t = self.tx_mut()?;
        let row = sqlx::query(
            r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type,
                      line_count, checksum_sha256, revision, ref_count,
                      last_embedded_content_hash, created_at, updated_at
               FROM veda_files WHERE id = ? FOR UPDATE"#,
        )
        .bind(file_id)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_file(&r)).transpose()
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
        .bind(db_enum_str(&file.storage_type))
        .bind(db_enum_str(&file.source_type))
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
        expected_rev: i32,
        new_rev: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
        mime_type: &str,
        source_type: SourceType,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        let r = sqlx::query(
            r#"UPDATE veda_files
               SET revision = ?, size_bytes = ?, checksum_sha256 = ?, line_count = ?, storage_type = ?, mime_type = ?, source_type = ?, last_embedded_content_hash = NULL
               WHERE id = ? AND revision = ?"#,
        )
        .bind(new_rev)
        .bind(size_bytes)
        .bind(checksum)
        .bind(line_count)
        .bind(db_enum_str(&storage_type))
        .bind(mime_type)
        .bind(db_enum_str(&source_type))
        .bind(file_id)
        .bind(expected_rev)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        if r.rows_affected() == 0 {
            return Err(VedaError::PreconditionFailed(format!(
                "file {file_id} revision mismatch (expected {expected_rev})"
            )));
        }
        Ok(())
    }

    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32> {
        let t = self.tx_mut()?;
        let row = sqlx::query(r#"SELECT ref_count FROM veda_files WHERE id = ? FOR UPDATE"#)
            .bind(file_id)
            .fetch_optional(t.as_mut())
            .await
            .map_err(storage_err)?;
        let r = row.ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;
        let current: i32 = r.try_get("ref_count").map_err(storage_err)?;
        if current <= 0 {
            return Err(VedaError::Internal(format!(
                "ref_count already {} for file {file_id}",
                current
            )));
        }
        let new_count = current - 1;
        sqlx::query(r#"UPDATE veda_files SET ref_count = ? WHERE id = ?"#)
            .bind(new_count)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(new_count)
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

    async fn get_file_content(&mut self, file_id: &str) -> Result<Option<String>> {
        let t = self.tx_mut()?;
        let row = sqlx::query(r#"SELECT content FROM veda_file_contents WHERE file_id = ?"#)
            .bind(file_id)
            .fetch_optional(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(row
            .map(|r| r.try_get::<String, _>("content"))
            .transpose()
            .map_err(storage_err)?)
    }

    async fn get_file_chunks(
        &mut self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let t = self.tx_mut()?;
        get_file_chunks_conn(t.as_mut(), file_id, start_line, end_line).await
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

    async fn insert_file_blob(&mut self, file_id: &str, data: &[u8]) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_file_blobs (file_id, data) VALUES (?, ?)
               ON DUPLICATE KEY UPDATE data = VALUES(data)"#,
        )
        .bind(file_id)
        .bind(data)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_blob(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_blobs WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_extract(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_extracts WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let t = self.tx_mut()?;
        for batch in chunks.chunks(CHUNK_INSERT_BATCH) {
            let placeholders: Vec<&str> = batch.iter().map(|_| "(?, ?, ?, ?, ?, ?, ?)").collect();
            let sql = format!(
                "INSERT INTO veda_file_chunks \
                 (file_id, chunk_index, start_line, line_count, byte_len, chunk_sha256, content) \
                 VALUES {}",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for c in batch {
                q = q
                    .bind(&c.file_id)
                    .bind(c.chunk_index)
                    .bind(c.start_line)
                    .bind(c.line_count)
                    .bind(c.byte_len)
                    .bind(&c.chunk_sha256)
                    .bind(&c.content);
            }
            q.execute(t.as_mut()).await.map_err(storage_err)?;
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

    async fn delete_file_chunks_from(
        &mut self,
        file_id: &str,
        from_chunk_index: i32,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_chunks WHERE file_id = ? AND chunk_index >= ?"#)
            .bind(file_id)
            .bind(from_chunk_index)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()> {
        let t = self.tx_mut()?;
        insert_outbox_conn(t.as_mut(), event).await
    }

    async fn try_insert_outbox_for_file(
        &mut self,
        event: &OutboxEvent,
        file_id: &str,
    ) -> Result<bool> {
        let t = self.tx_mut()?;
        let et = db_enum_str(&event.event_type);
        // Only deduplicate against `pending` events. A `processing` event is
        // already in flight against an older snapshot of the file; if we
        // dedupe against it, the new content's ChunkSync gets dropped and the
        // worker silently embeds stale data. Letting it through means after
        // the in-flight task completes, the worker picks up the new pending
        // event; if the second event's content_hash equals the watermark by
        // then (e.g. user reverted), `handle_chunk_sync` short-circuits.
        let row: Option<(i64,)> = sqlx::query_as(
            r#"SELECT 1 FROM veda_outbox
               WHERE event_type = ? AND workspace_id = ? AND status = 'pending'
                 AND JSON_UNQUOTE(JSON_EXTRACT(payload, '$.file_id')) = ?
               LIMIT 1"#,
        )
        .bind(et)
        .bind(&event.workspace_id)
        .bind(file_id)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        if row.is_some() {
            return Ok(false);
        }
        insert_outbox_conn(t.as_mut(), event).await?;
        Ok(true)
    }

    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> {
        let t = self.tx_mut()?;
        insert_fs_event_conn(t.as_mut(), event).await
    }

    async fn insert_fs_events(&mut self, events: &[FsEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let t = self.tx_mut()?;
        for batch in events.chunks(FS_EVENT_INSERT_BATCH) {
            let placeholders: Vec<&str> = batch.iter().map(|_| "(?, ?, ?, ?, ?)").collect();
            let sql = format!(
                "INSERT INTO veda_fs_events (workspace_id, event_type, path, file_id, created_at) VALUES {}",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for e in batch {
                q = q
                    .bind(&e.workspace_id)
                    .bind(db_enum_str(&e.event_type))
                    .bind(&e.path)
                    .bind(&e.file_id)
                    .bind(e.created_at.naive_utc());
            }
            q.execute(t.as_mut()).await.map_err(storage_err)?;
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

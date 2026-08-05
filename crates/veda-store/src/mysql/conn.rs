use super::*;

pub(super) async fn get_dentry_conn(
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

pub(super) async fn get_file_conn(
    conn: &mut sqlx::MySqlConnection,
    file_id: &str,
) -> Result<Option<FileRecord>> {
    let row = sqlx::query(
        r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
                  checksum_sha256, revision, ref_count, last_embedded_content_hash,
                  created_at, updated_at
           FROM veda_files WHERE id = ?"#,
    )
    .bind(file_id)
    .fetch_optional(conn)
    .await
    .map_err(storage_err)?;
    row.map(|r| row_to_file(&r)).transpose()
}

pub(super) async fn list_dentries_under_page_conn(
    conn: &mut sqlx::MySqlConnection,
    workspace_id: &str,
    path_prefix: &str,
    after_path: Option<&str>,
    limit: usize,
) -> Result<Vec<Dentry>> {
    // Empty string sorts before every real path (paths begin with '/'),
    // so `path > ''` is equivalent to "no cursor". This collapses the
    // None / Some(..) split to a single SQL statement.
    let after = after_path.unwrap_or("");
    let limit_i64 = limit as i64;
    let rows = if path_prefix == "/" {
        sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries
               WHERE workspace_id = ? AND path > ?
               ORDER BY path
               LIMIT ?"#,
        )
        .bind(workspace_id)
        .bind(after)
        .bind(limit_i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage_err)?
    } else {
        let like = format!("{}/%", escape_like(path_prefix));
        sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries
               WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\' AND path > ?
               ORDER BY path
               LIMIT ?"#,
        )
        .bind(workspace_id)
        .bind(&like)
        .bind(after)
        .bind(limit_i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage_err)?
    };
    rows.iter().map(|r| row_to_dentry(r)).collect()
}

pub(super) async fn get_file_chunks_conn(
    conn: &mut sqlx::MySqlConnection,
    file_id: &str,
    start_line: Option<i32>,
    end_line: Option<i32>,
) -> Result<Vec<FileChunk>> {
    let mut q = String::from(
        r#"SELECT file_id, chunk_index, start_line, line_count, byte_len, chunk_sha256, content FROM veda_file_chunks WHERE file_id = ?"#,
    );
    let rows = match (start_line, end_line) {
        (Some(a), Some(b)) => {
            // The inner subquery picks the chunk that *starts at or before* line a
            // (highest chunk_index with start_line <= a). When `a` is past EOF, this
            // still returns the LAST chunk — which doesn't actually contain line a.
            // The outer `start_line + line_count >= ?` filter excludes chunks whose
            // line range ends before a. We use `>=` (not `>`) because line_count is
            // the count of '\n' bytes inside the chunk: when the file has no
            // trailing newline, the final logical line satisfies
            // `start_line + line_count == last_line`, and `>` would drop it.
            q.push_str(
                " AND chunk_index >= COALESCE((SELECT MAX(chunk_index) \
                   FROM veda_file_chunks WHERE file_id = ? AND start_line <= ?), 0) \
                   AND start_line + line_count >= ? \
                   AND start_line <= ? ORDER BY chunk_index",
            );
            sqlx::query(&q)
                .bind(file_id)
                .bind(file_id)
                .bind(a)
                .bind(a)
                .bind(b)
                .fetch_all(&mut *conn)
                .await
        }
        (Some(a), None) => {
            q.push_str(
                " AND chunk_index >= COALESCE((SELECT MAX(chunk_index) \
                   FROM veda_file_chunks WHERE file_id = ? AND start_line <= ?), 0) \
                   AND start_line + line_count >= ? \
                   ORDER BY chunk_index",
            );
            sqlx::query(&q)
                .bind(file_id)
                .bind(file_id)
                .bind(a)
                .bind(a)
                .fetch_all(&mut *conn)
                .await
        }
        (None, Some(b)) => {
            q.push_str(" AND start_line <= ? ORDER BY chunk_index");
            sqlx::query(&q)
                .bind(file_id)
                .bind(b)
                .fetch_all(&mut *conn)
                .await
        }
        (None, None) => {
            q.push_str(" ORDER BY chunk_index");
            sqlx::query(&q).bind(file_id).fetch_all(&mut *conn).await
        }
    }
    .map_err(storage_err)?;
    rows.iter().map(|r| row_to_file_chunk(r)).collect()
}

pub(super) async fn insert_fs_event_conn(conn: &mut sqlx::MySqlConnection, event: &FsEvent) -> Result<()> {
    let et = db_enum_str(&event.event_type);
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
        .execute(&mut *conn)
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
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

pub(super) async fn insert_outbox_conn(conn: &mut sqlx::MySqlConnection, event: &OutboxEvent) -> Result<()> {
    use chrono::Timelike;
    let payload = serde_json::to_string(&event.payload).map_err(|e| storage_err(e.to_string()))?;
    let status = db_enum_str(&event.status);
    let et = db_enum_str(&event.event_type);
    // MySQL ROUNDS fractional seconds into TIMESTAMP(0): an available_at of
    // ...39.7 lands as ...40, putting a just-enqueued task up to 500ms in the
    // future where claim's `available_at <= UTC_TIMESTAMP()` can't see it.
    // Truncate so enqueue-then-claim is immediate.
    let available_at = event
        .available_at
        .naive_utc()
        .with_nanosecond(0)
        .expect("nanosecond 0 is always valid");
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
        .bind(available_at)
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
        .bind(available_at)
        .bind(event.lease_until.map(|x| x.naive_utc()))
        .bind(event.created_at.naive_utc())
        .execute(conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}


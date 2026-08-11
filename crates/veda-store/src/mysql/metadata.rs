use super::*;

#[async_trait]
impl MetadataStore for MysqlStore {
    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn count_index_backlog(&self, workspace_id: &str) -> Result<(i64, i64, i64)> {
        // Rides idx_dedup (workspace_id, event_type, status) — never a
        // table scan even on a fat outbox.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT status, COUNT(*) FROM veda_outbox
               WHERE workspace_id = ?
                 AND event_type IN ('chunk_sync', 'extract_sync')
                 AND status IN ('pending', 'processing', 'dead')
               GROUP BY status"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut pending = 0;
        let mut processing = 0;
        let mut dead = 0;
        for (status, n) in rows {
            match status.as_str() {
                "pending" => pending = n,
                "processing" => processing = n,
                "dead" => dead = n,
                _ => {}
            }
        }
        Ok((pending, processing, dead))
    }

    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        get_dentry_conn(&mut *conn, workspace_id, path).await
    }

    async fn insert_dentry_ignore(&self, dentry: &Dentry) -> Result<()> {
        match sqlx::query(
            r#"INSERT IGNORE INTO veda_dentries
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
        .execute(&self.pool)
        .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(storage_err(e)),
        }
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

    async fn list_children_capped(
        &self,
        workspace_id: &str,
        parent_path: &str,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        // ORDER BY is_dir DESC puts directories first so that truncation at
        // `limit` keeps the entries worth naming. Uses idx_parent
        // (workspace_id, parent_path(255)).
        //
        // COLLATE utf8mb4_bin because the column's collation is
        // utf8mb4_0900_ai_ci: under it `/Docs` and `/docs` compare EQUAL, so
        // their relative order is unspecified and a truncation boundary
        // landing between them would return different rows run to run.
        // Binary collation makes this a total order.
        let mut rows = sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries WHERE workspace_id = ? AND parent_path = ?
               ORDER BY is_dir DESC, path COLLATE utf8mb4_bin
               LIMIT ?"#,
        )
        .bind(workspace_id)
        .bind(parent_path)
        .bind(limit as u64)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows.drain(..) {
            out.push(row_to_dentry(&r)?);
        }
        Ok(out)
    }

    async fn count_files_by_top_level(
        &self,
        workspace_id: &str,
    ) -> Result<std::collections::HashMap<String, i64>> {
        // Paths are normalised absolute ("/docs/a/b.md"), so dropping the
        // leading slash and taking the first segment yields the top-level
        // area. A root-level file ("/README.md") groups under its own name —
        // callers must only read this map for entries where is_dir is true.
        //
        // The grouping deliberately inherits the column's own
        // utf8mb4_0900_ai_ci collation, which folds case and accents. That
        // matches how the rest of veda compares paths: `get_dentry` and
        // `list_dentries` both do a plain `path = ?` / `parent_path = ?`
        // against this column, so `/Docs` and `/docs` already resolve to one
        // directory and a listing of it returns files written under either
        // spelling. Forcing a binary collation here would split the count
        // while `list_dir` kept showing the union — the layout would then
        // disagree with the directory it describes.
        let rows = sqlx::query(
            r#"SELECT SUBSTRING_INDEX(SUBSTRING(path, 2), '/', 1) AS top_seg, COUNT(*) AS n
               FROM veda_dentries
               WHERE workspace_id = ? AND is_dir = false
               GROUP BY top_seg"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut map = std::collections::HashMap::new();
        for r in &rows {
            let seg: String = r.try_get("top_seg").map_err(storage_err)?;
            let n: i64 = r.try_get("n").map_err(storage_err)?;
            map.insert(seg, n);
        }
        Ok(map)
    }

    async fn sum_bytes_by_child(
        &self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<std::collections::HashMap<String, i64>> {
        // Same expression-grouping shape (and collation caveats) as
        // `count_files_by_top_level` above, scoped to one parent. The
        // child segment starts right after "parent/" — position is
        // computed in Rust and bound as a parameter so the SQL stays a
        // single static statement for both root and nested parents.
        let (like, seg_start) = if parent_path == "/" {
            ("/%".to_string(), 2i64)
        } else {
            (
                format!("{}/%", escape_like(parent_path)),
                parent_path.chars().count() as i64 + 2,
            )
        };
        let rows = sqlx::query(
            r#"SELECT SUBSTRING_INDEX(SUBSTRING(d.path, ?), '/', 1) AS child,
                      CAST(COALESCE(SUM(f.size_bytes), 0) AS SIGNED) AS bytes
               FROM veda_dentries d
               JOIN veda_files f ON d.file_id = f.id
               WHERE d.workspace_id = ? AND d.is_dir = false AND d.path LIKE ? ESCAPE '\\'
               GROUP BY child"#,
        )
        .bind(seg_start)
        .bind(workspace_id)
        .bind(&like)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut map = std::collections::HashMap::new();
        for r in &rows {
            let seg: String = r.try_get("child").map_err(storage_err)?;
            let n: i64 = r.try_get("bytes").map_err(storage_err)?;
            map.insert(seg, n);
        }
        Ok(map)
    }

    async fn list_dentries_under_page(
        &self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        list_dentries_under_page_conn(
            &mut *conn,
            workspace_id,
            path_prefix,
            after_path,
            limit,
        )
        .await
    }

    async fn get_file(&self, file_id: &str) -> Result<Option<FileRecord>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        get_file_conn(&mut *conn, file_id).await
    }

    async fn get_files_batch(&self, file_ids: &[String]) -> Result<Vec<FileRecord>> {
        if file_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = vec!["?"; file_ids.len()].join(",");
        let sql = format!(
            "SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, \
             line_count, checksum_sha256, revision, ref_count, last_embedded_content_hash, \
             created_at, updated_at \
             FROM veda_files WHERE id IN ({})",
            placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in file_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        rows.iter().map(|r| row_to_file(r)).collect()
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

    async fn get_file_blob(&self, file_id: &str) -> Result<Option<Vec<u8>>> {
        let row = sqlx::query(r#"SELECT data FROM veda_file_blobs WHERE file_id = ?"#)
            .bind(file_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(row
            .map(|r| r.try_get::<Vec<u8>, _>("data"))
            .transpose()
            .map_err(storage_err)?)
    }

    async fn get_file_extract(&self, file_id: &str) -> Result<Option<FileExtract>> {
        let row = sqlx::query(
            r#"SELECT content, source_sha256 FROM veda_file_extracts WHERE file_id = ?"#,
        )
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| {
            Ok(FileExtract {
                file_id: file_id.to_string(),
                content: r.try_get("content").map_err(storage_err)?,
                source_sha256: r.try_get("source_sha256").map_err(storage_err)?,
            })
        })
        .transpose()
    }

    async fn upsert_file_extract(&self, extract: &FileExtract) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO veda_file_extracts (file_id, content, source_sha256)
               VALUES (?, ?, ?)
               ON DUPLICATE KEY UPDATE content = VALUES(content),
                                       source_sha256 = VALUES(source_sha256)"#,
        )
        .bind(&extract.file_id)
        .bind(&extract.content)
        .bind(&extract.source_sha256)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_extract(&self, file_id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_file_extracts WHERE file_id = ?"#)
            .bind(file_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn get_file_chunks(
        &self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        get_file_chunks_conn(&mut *conn, file_id, start_line, end_line).await
    }

    async fn list_chunk_byte_lens(&self, file_id: &str) -> Result<Vec<(i32, i32)>> {
        let rows = sqlx::query(
            r#"SELECT chunk_index, byte_len FROM veda_file_chunks
               WHERE file_id = ? ORDER BY chunk_index"#,
        )
        .bind(file_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter()
            .map(|r| {
                let idx: i32 = r.try_get("chunk_index").map_err(storage_err)?;
                let len: i32 = r.try_get("byte_len").map_err(storage_err)?;
                Ok((idx, len))
            })
            .collect()
    }

    async fn get_chunks_in_index_range(
        &self,
        file_id: &str,
        idx_min: i32,
        idx_max: i32,
    ) -> Result<Vec<FileChunk>> {
        let rows = sqlx::query(
            r#"SELECT file_id, chunk_index, start_line, line_count, byte_len, chunk_sha256, content
               FROM veda_file_chunks
               WHERE file_id = ? AND chunk_index >= ? AND chunk_index <= ?
               ORDER BY chunk_index"#,
        )
        .bind(file_id)
        .bind(idx_min)
        .bind(idx_max)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter().map(|r| row_to_file_chunk(r)).collect()
    }

    async fn find_file_by_checksum(
        &self,
        workspace_id: &str,
        checksum: &str,
    ) -> Result<Option<FileRecord>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
                      checksum_sha256, revision, ref_count, last_embedded_content_hash,
                      created_at, updated_at
               FROM veda_files WHERE workspace_id = ? AND checksum_sha256 = ? LIMIT 1"#,
        )
        .bind(workspace_id)
        .bind(checksum)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_file(&r)).transpose()
    }

    async fn get_dentry_path_by_file_id(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT path FROM veda_dentries WHERE workspace_id = ? AND file_id = ? LIMIT 1",
        )
        .bind(workspace_id)
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(row.map(|r| r.0))
    }

    async fn get_dentry_paths_by_file_ids(
        &self,
        workspace_id: &str,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, DentryPathRef>> {
        if file_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = vec!["?"; file_ids.len()].join(",");
        // ORDER BY path + or_insert keeps the smallest path per file_id, so
        // copy-alias attribution is deterministic across queries (trait
        // contract on DentryPathRef). The `id` tie-break matters: the path
        // column's collation is CI (see todos: collation not pinned), so
        // case-only aliases like /A vs /a sort EQUAL and MySQL would return
        // them in arbitrary order without it.
        let sql = format!(
            "SELECT id, file_id, path FROM veda_dentries \
             WHERE workspace_id = ? AND file_id IN ({placeholders}) \
             ORDER BY path, id"
        );
        let mut q = sqlx::query(&sql).bind(workspace_id);
        for id in file_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let dentry_id: String = r.try_get("id").map_err(storage_err)?;
            let fid: String = r.try_get("file_id").map_err(storage_err)?;
            let path: String = r.try_get("path").map_err(storage_err)?;
            map.entry(fid).or_insert(DentryPathRef { dentry_id, path });
        }
        Ok(map)
    }

    async fn get_dentry_paths_by_ids(
        &self,
        workspace_id: &str,
        dentry_ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        if dentry_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = vec!["?"; dentry_ids.len()].join(",");
        let sql = format!(
            "SELECT id, path FROM veda_dentries \
             WHERE workspace_id = ? AND id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(workspace_id);
        for id in dentry_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").map_err(storage_err)?;
            let path: String = r.try_get("path").map_err(storage_err)?;
            map.insert(id, path);
        }
        Ok(map)
    }

    async fn upsert_doc_access_daily(&self, rows: &[DocAccessRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        // One transaction for the whole delta set. Chunked multi-value
        // INSERTs keep statements bounded; atomicity keeps retry semantics
        // simple for the caller (all-or-nothing, no double-count on retry).
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        for chunk in rows.chunks(500) {
            let values = vec!["(?,?,?,?,?)"; chunk.len()].join(",");
            // Column is `read_count`, not `reads` — READS is a MySQL
            // reserved word (caught by migrate against a real 8.0 server).
            let sql = format!(
                "INSERT INTO veda_doc_access_daily \
                 (workspace_id, day, dentry_id, search_hits, read_count) \
                 VALUES {values} \
                 ON DUPLICATE KEY UPDATE \
                 search_hits = search_hits + VALUES(search_hits), \
                 read_count = read_count + VALUES(read_count)"
            );
            let mut q = sqlx::query(&sql);
            for r in chunk {
                q = q
                    .bind(&r.workspace_id)
                    .bind(r.day)
                    .bind(&r.dentry_id)
                    .bind(r.search_hits)
                    .bind(r.reads);
            }
            q.execute(&mut *tx).await.map_err(storage_err)?;
        }
        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn query_doc_access(
        &self,
        workspace_id: &str,
        since: chrono::NaiveDate,
        order: DocAccessOrder,
        limit: usize,
    ) -> Result<Vec<veda_types::api::DocAccessEntry>> {
        let order_col = match order {
            DocAccessOrder::Reads => "read_count",
            DocAccessOrder::SearchHits => "hit_count",
        };
        // INNER JOIN against live dentries: deleted docs drop off the board,
        // renames stay continuous (dentry_id survives them).
        let sql = format!(
            "SELECT d.path AS path, \
             CAST(SUM(s.search_hits) AS UNSIGNED) AS hit_count, \
             CAST(SUM(s.read_count) AS UNSIGNED) AS read_count \
             FROM veda_doc_access_daily s \
             JOIN veda_dentries d \
               ON d.id = s.dentry_id AND d.workspace_id = s.workspace_id \
             WHERE s.workspace_id = ? AND s.day >= ? \
             GROUP BY s.dentry_id, d.path \
             ORDER BY {order_col} DESC, path ASC \
             LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(workspace_id)
            .bind(since)
            .bind(i64::try_from(limit).unwrap_or(200))
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        rows.iter()
            .map(|r| {
                Ok(veda_types::api::DocAccessEntry {
                    path: r.try_get("path").map_err(storage_err)?,
                    search_hits: r.try_get("hit_count").map_err(storage_err)?,
                    reads: r.try_get("read_count").map_err(storage_err)?,
                })
            })
            .collect()
    }

    async fn sweep_doc_access(&self, cutoff: chrono::NaiveDate) -> Result<u64> {
        // Same chunked-delete shape as prune_fs_events_older_than: bounded
        // lock lists, yield between chunks so live flushes interleave.
        const CHUNK: u64 = 5000;
        let mut total = 0u64;
        loop {
            let r = sqlx::query("DELETE FROM veda_doc_access_daily WHERE day < ? LIMIT 5000")
                .bind(cutoff)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            let n = r.rows_affected();
            total += n;
            if n < CHUNK {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok(total)
    }

    async fn query_fs_events(
        &self,
        workspace_id: &str,
        since_id: i64,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FsEvent>> {
        let limit_i64 = i64::try_from(limit).unwrap_or(10_000);
        // path_prefix is treated as a directory subtree. We match `path = prefix`
        // (the dir entry itself, if any) OR `path LIKE 'prefix/%'`. The naive
        // `LIKE 'prefix%'` form would leak into siblings (e.g. `/docs_alt/*`
        // when the user asked for `/docs`), which is a hard correctness bug for
        // any caller wiring this up to an authorization or notification fence.
        // `/` is special-cased upstream (treated as unfiltered) and never reaches
        // this branch with a meaningful trailing slash.
        let rows = match path_prefix {
            Some("/") => {
                sqlx::query(
                    r#"SELECT id, workspace_id, event_type, path, file_id, created_at
                       FROM veda_fs_events
                       WHERE workspace_id = ? AND id > ?
                       ORDER BY id ASC LIMIT ?"#,
                )
                .bind(workspace_id)
                .bind(since_id)
                .bind(limit_i64)
                .fetch_all(&self.pool)
                .await
                .map_err(storage_err)?
            }
            Some(prefix) => {
                let prefix = prefix.trim_end_matches('/');
                let subtree_like = format!("{}/%", escape_like(prefix));
                sqlx::query(
                    r#"SELECT id, workspace_id, event_type, path, file_id, created_at
                       FROM veda_fs_events
                       WHERE workspace_id = ? AND id > ?
                         AND (path = ? OR path LIKE ? ESCAPE '\\')
                       ORDER BY id ASC LIMIT ?"#,
                )
                .bind(workspace_id)
                .bind(since_id)
                .bind(prefix)
                .bind(&subtree_like)
                .bind(limit_i64)
                .fetch_all(&self.pool)
                .await
                .map_err(storage_err)?
            }
            None => sqlx::query(
                r#"SELECT id, workspace_id, event_type, path, file_id, created_at
                       FROM veda_fs_events
                       WHERE workspace_id = ? AND id > ?
                       ORDER BY id ASC LIMIT ?"#,
            )
            .bind(workspace_id)
            .bind(since_id)
            .bind(limit_i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?,
        };
        rows.iter().map(|r| row_to_fs_event(r)).collect()
    }

    async fn min_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>> {
        let row = sqlx::query(
            r#"SELECT MIN(id) AS min_id FROM veda_fs_events WHERE workspace_id = ?"#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;
        // MIN over an empty set is NULL — try_get returns Err for NULL, so
        // map that to None instead of bubbling up the type-coercion error.
        Ok(row.try_get::<i64, _>("min_id").ok())
    }

    async fn max_fs_event_id(&self, workspace_id: &str) -> Result<Option<i64>> {
        let row = sqlx::query(
            r#"SELECT MAX(id) AS max_id FROM veda_fs_events WHERE workspace_id = ?"#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(row.try_get::<i64, _>("max_id").ok())
    }

    async fn prune_fs_events_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        // Batched delete: a single unbounded DELETE on a large event table
        // would grab a lock-list proportional to the matching row count and
        // can stall live writers for tens of seconds. We chunk by 5000 rows
        // and yield between iterations so live `INSERT INTO veda_fs_events`
        // can interleave. The loop terminates when a chunk affects 0 rows.
        const CHUNK: u64 = 5000;
        let mut total = 0u64;
        loop {
            let r = sqlx::query(r#"DELETE FROM veda_fs_events WHERE created_at < ? LIMIT 5000"#)
                .bind(cutoff.naive_utc())
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            let n = r.rows_affected();
            total += n;
            if n < CHUNK {
                break;
            }
            // Yield to the runtime so other queries on the pool can interleave
            // between chunks. No backoff — we're disk- and lock-bound, not CPU.
            tokio::task::yield_now().await;
        }
        Ok(total)
    }

    async fn insert_fs_event_direct(&self, event: &FsEvent) -> Result<()> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        insert_fs_event_conn(&mut conn, event).await
    }

    async fn storage_stats(&self, workspace_id: &str) -> Result<StorageStats> {
        let row = sqlx::query(
            r#"SELECT
                COUNT(CASE WHEN d.is_dir = false THEN 1 END) AS total_files,
                COUNT(CASE WHEN d.is_dir = true THEN 1 END) AS total_directories,
                CAST(COALESCE(SUM(f.size_bytes), 0) AS SIGNED) AS total_bytes
               FROM veda_dentries d
               LEFT JOIN veda_files f ON d.file_id = f.id
               WHERE d.workspace_id = ?"#,
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;

        Ok(StorageStats {
            total_files: row.try_get::<i64, _>("total_files").unwrap_or(0),
            total_directories: row.try_get::<i64, _>("total_directories").unwrap_or(0),
            // Surface decode errors instead of silently swallowing to 0. The
            // CAST(... AS SIGNED) above keeps this an i64-decodable column;
            // the prior COALESCE(SUM(...)) returned DECIMAL, which
            // try_get::<i64> rejected and unwrap_or(0) hid → always-0 bytes.
            total_bytes: row.try_get::<i64, _>("total_bytes").map_err(storage_err)?,
        })
    }

    async fn update_file_content_hash(&self, file_id: &str, hash: &str) -> Result<()> {
        sqlx::query(
            r#"UPDATE veda_files SET last_embedded_content_hash = ? WHERE id = ?"#,
        )
        .bind(hash)
        .bind(file_id)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>> {
        let tx = self
            .pool
            .begin()
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(Box::new(MysqlMetadataTx { tx: Some(tx) }))
    }

    async fn get_summary_by_file(&self, file_id: &str) -> Result<Option<FileSummary>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview,
                      status, created_at, updated_at
               FROM veda_summaries WHERE file_id = ?"#,
        )
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_summary(&r)).transpose()
    }

    async fn get_summaries_by_file_ids(
        &self,
        file_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FileSummary>> {
        if file_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = vec!["?"; file_ids.len()].join(",");
        let sql = format!(
            r#"SELECT id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview,
                      status, created_at, updated_at
               FROM veda_summaries WHERE file_id IN ({placeholders})"#
        );
        let mut q = sqlx::query(&sql);
        for fid in file_ids {
            q = q.bind(fid);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        let mut map = std::collections::HashMap::new();
        for r in &rows {
            let s = row_to_summary(r)?;
            if let Some(fid) = &s.file_id {
                map.insert(fid.clone(), s);
            }
        }
        Ok(map)
    }

    async fn get_summaries_by_dentry_ids(
        &self,
        dentry_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FileSummary>> {
        if dentry_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = vec!["?"; dentry_ids.len()].join(",");
        let sql = format!(
            r#"SELECT id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview,
                      status, created_at, updated_at
               FROM veda_summaries WHERE dentry_id IN ({placeholders})"#
        );
        let mut q = sqlx::query(&sql);
        for did in dentry_ids {
            q = q.bind(did);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        let mut map = std::collections::HashMap::new();
        for r in &rows {
            let s = row_to_summary(r)?;
            if let Some(did) = &s.dentry_id {
                map.insert(did.clone(), s);
            }
        }
        Ok(map)
    }

    async fn get_summary_by_dentry(&self, dentry_id: &str) -> Result<Option<FileSummary>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview,
                      status, created_at, updated_at
               FROM veda_summaries WHERE dentry_id = ?"#,
        )
        .bind(dentry_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_summary(&r)).transpose()
    }

    async fn list_ready_summary_keys(
        &self,
        workspace_id: &str,
    ) -> Result<(
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    )> {
        use sqlx::Row;
        let rows = sqlx::query(
            r#"SELECT file_id, dentry_id
               FROM veda_summaries
               WHERE workspace_id = ? AND status = 'ready'"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut file_ids = std::collections::HashSet::new();
        let mut dentry_ids = std::collections::HashSet::new();
        for r in &rows {
            if let Ok(Some(fid)) = r.try_get::<Option<String>, _>("file_id") {
                file_ids.insert(fid);
            }
            if let Ok(Some(did)) = r.try_get::<Option<String>, _>("dentry_id") {
                dentry_ids.insert(did);
            }
        }
        Ok((file_ids, dentry_ids))
    }

    async fn upsert_summary(&self, summary: &FileSummary) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO veda_summaries (id, workspace_id, file_id, dentry_id, l0_abstract, l1_overview, status)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               ON DUPLICATE KEY UPDATE
                 l0_abstract = VALUES(l0_abstract),
                 l1_overview = VALUES(l1_overview),
                 status = VALUES(status)"#,
        )
        .bind(&summary.id)
        .bind(&summary.workspace_id)
        .bind(&summary.file_id)
        .bind(&summary.dentry_id)
        .bind(&summary.l0_abstract)
        .bind(&summary.l1_overview)
        .bind(db_enum_str(&summary.status))
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_summary_by_file(&self, file_id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_summaries WHERE file_id = ?"#)
            .bind(file_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_summary_by_dentry(&self, dentry_id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_summaries WHERE dentry_id = ?"#)
            .bind(dentry_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_child_summaries(
        &self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<Vec<FileSummary>> {
        let rows = sqlx::query(
            r#"SELECT s.id, s.workspace_id, s.file_id, s.dentry_id, s.l0_abstract, s.l1_overview,
                      s.status, s.created_at, s.updated_at
               FROM veda_summaries s
               INNER JOIN veda_dentries d ON s.file_id = d.file_id
               WHERE s.file_id IS NOT NULL AND d.workspace_id = ? AND d.parent_path = ? AND s.status = 'ready'
             UNION ALL
             SELECT s.id, s.workspace_id, s.file_id, s.dentry_id, s.l0_abstract, s.l1_overview,
                      s.status, s.created_at, s.updated_at
               FROM veda_summaries s
               INNER JOIN veda_dentries d ON s.dentry_id = d.id
               WHERE s.dentry_id IS NOT NULL AND d.workspace_id = ? AND d.parent_path = ? AND s.status = 'ready'"#,
        )
        .bind(workspace_id)
        .bind(parent_path)
        .bind(workspace_id)
        .bind(parent_path)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter().map(|r| row_to_summary(r)).collect()
    }
}

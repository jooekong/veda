use super::*;

/// Outbox lease duration. Workers heartbeat-renew at a fraction of this
/// (veda-server `LEASE_RENEW_INTERVAL`), so a lease only expires when its
/// holder stopped renewing for the whole window — i.e. crashed or was
/// SIGKILLed — after which a later claim retries the row.
const OUTBOX_LEASE_MINUTES: i32 = 10;

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
               WHERE (status = 'pending' AND available_at <= UTC_TIMESTAMP())
                  OR (status = 'processing' AND lease_until IS NOT NULL AND lease_until <= UTC_TIMESTAMP())
               ORDER BY id ASC
               LIMIT ?
               FOR UPDATE SKIP LOCKED"#,
        )
        .bind(batch_size_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage_err)?;
        let mut events = Vec::new();
        let mut dead_ids: Vec<(i64, String)> = Vec::new();
        let mut reclaims: Vec<(i64, String)> = Vec::new();
        for r in &rows {
            // An unparsable row (typically an event_type enum this binary
            // predates, i.e. running an older build after a rollback) must
            // not poison the whole batch: `?` here would abort the claim
            // transaction every cycle and stall the entire outbox. Dead-
            // letter the row alone and keep claiming — redrive after
            // upgrading back is a manual UPDATE.
            let mut evt = match row_to_outbox(r) {
                Ok(e) => e,
                Err(err) => {
                    if let Ok(id) = r.try_get::<i64, _>("id") {
                        tracing::warn!(task_id = id, err = %err, "outbox row unparsable, dead-lettering");
                        dead_ids.push((id, "unparsable".to_string()));
                    }
                    continue;
                }
            };
            let was_processing = evt.status == OutboxStatus::Processing;
            if was_processing {
                // Lease expired: previous attempt crashed without calling fail(),
                // so count it here. fail() resets status to 'pending', so next
                // claim() won't enter this branch — no double-increment.
                let next_retry = evt.retry_count + 1;
                if next_retry >= evt.max_retries {
                    dead_ids.push((evt.id, db_enum_str(&evt.event_type)));
                    continue;
                }
                reclaims.push((evt.id, db_enum_str(&evt.event_type)));
                sqlx::query(
                    r#"UPDATE veda_outbox SET status = 'processing', retry_count = ?,
                       lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE)
                       WHERE id = ?"#,
                )
                .bind(next_retry)
                .bind(OUTBOX_LEASE_MINUTES)
                .bind(evt.id)
                .execute(&mut *tx)
                .await
                .map_err(storage_err)?;
                // Keep the returned event in sync with what was just
                // persisted — callers (and tests) see the real retry budget.
                evt.retry_count = next_retry;
            } else {
                sqlx::query(
                    r#"UPDATE veda_outbox SET status = 'processing',
                       lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE)
                       WHERE id = ?"#,
                )
                .bind(OUTBOX_LEASE_MINUTES)
                .bind(evt.id)
                .execute(&mut *tx)
                .await
                .map_err(storage_err)?;
            }
            events.push(evt);
        }
        for (id, _event_type) in &dead_ids {
            sqlx::query(
                r#"UPDATE veda_outbox SET status = 'dead', lease_until = NULL WHERE id = ?"#,
            )
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(storage_err)?;
        }
        tx.commit().await.map_err(storage_err)?;
        // Surface dead-letter now that the transition is durable. This
        // lease-expiry path bypasses fail(), so without this it is fully
        // silent — no log, no metric (review H4).
        for (id, event_type) in &dead_ids {
            tracing::warn!(
                task_id = *id,
                event_type = %event_type,
                "outbox task dead: lease expired past max_retries"
            );
            ::metrics::counter!("veda_outbox_dead_total", "event_type" => event_type.clone())
                .increment(1);
        }
        // A reclaim means the previous attempt stopped heartbeating (crash /
        // SIGKILL / restart mid-batch); the row is retried here.
        for (id, event_type) in &reclaims {
            tracing::warn!(
                task_id = *id,
                event_type = %event_type,
                "outbox lease expired; task reclaimed for retry"
            );
        }
        Ok(events)
    }

    async fn complete(&self, task_id: i64) -> Result<()> {
        let res = sqlx::query(
            r#"UPDATE veda_outbox SET status = 'completed', lease_until = NULL
               WHERE id = ? AND status = 'processing'"#,
        )
        .bind(task_id)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        if res.rows_affected() == 0 {
            // Row is no longer processing: duplicate completion, or the row
            // was dead-lettered / re-driven after its lease expired.
            tracing::warn!(task_id, "outbox complete dropped: task no longer processing");
        }
        Ok(())
    }

    async fn fail(&self, task_id: i64, error: &str) -> Result<()> {
        // The SELECT→UPDATE pair is not transactional; a state change in
        // between is caught by the status condition on the UPDATEs below
        // (rows_affected = 0).
        let row = sqlx::query(
            r#"SELECT id, retry_count, max_retries, payload, event_type FROM veda_outbox
               WHERE id = ? AND status = 'processing'"#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        let Some(r) = row else {
            tracing::warn!(task_id, "outbox fail dropped: task no longer processing");
            return Ok(());
        };
        let retry: i32 = r.try_get("retry_count").map_err(storage_err)?;
        let max: i32 = r.try_get("max_retries").map_err(storage_err)?;
        let event_type: String = r.try_get("event_type").map_err(storage_err)?;
        let Json(mut payload): Json<serde_json::Value> =
            r.try_get("payload").map_err(storage_err)?;
        if let serde_json::Value::Object(ref mut m) = payload {
            m.insert(
                "_last_error".into(),
                serde_json::Value::String(error.to_string()),
            );
        }
        let payload_str =
            serde_json::to_string(&payload).map_err(|e| storage_err(e.to_string()))?;
        let next_retry = retry + 1;
        if next_retry >= max {
            // Status fencing makes the terminal transition idempotent: a
            // state change between the SELECT above and here (e.g. claim()
            // already dead-lettered the row) leaves rows_affected = 0 and
            // the dead counter exact.
            let res = sqlx::query(
                r#"UPDATE veda_outbox SET status = 'dead', retry_count = ?, payload = CAST(? AS JSON),
                   lease_until = NULL
                   WHERE id = ? AND status = 'processing'"#,
            )
            .bind(next_retry)
            .bind(&payload_str)
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
            // Only count the death THIS call actually performed. worker.rs
            // already logged + bumped veda_outbox_failed_total for this
            // attempt; this is the dedicated dead-letter counter ops alert on
            // (H4).
            if res.rows_affected() > 0 {
                tracing::warn!(task_id, event_type = %event_type, "outbox task dead: retries exhausted");
                ::metrics::counter!("veda_outbox_dead_total", "event_type" => event_type)
                    .increment(1);
            } else {
                tracing::warn!(task_id, "outbox fail dropped: task no longer processing");
            }
        } else {
            let backoff_secs: i64 = (30 * (1i64 << next_retry.min(10))).min(3600);
            let res = sqlx::query(
                "UPDATE veda_outbox SET status = 'pending', retry_count = ?, payload = CAST(? AS JSON), \
                 available_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? SECOND), lease_until = NULL \
                 WHERE id = ? AND status = 'processing'",
            )
                .bind(next_retry)
                .bind(&payload_str)
                .bind(backoff_secs)
                .bind(task_id)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            if res.rows_affected() == 0 {
                tracing::warn!(task_id, "outbox fail dropped: task no longer processing");
            }
        }
        Ok(())
    }

    async fn renew(&self, task_ids: &[i64]) -> Result<()> {
        if task_ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; task_ids.len()].join(",");
        let sql = format!(
            "UPDATE veda_outbox SET lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE) \
             WHERE status = 'processing' AND id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(OUTBOX_LEASE_MINUTES);
        for id in task_ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await.map_err(storage_err)?;
        Ok(())
    }

    async fn prune_outbox_older_than(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        // Mirror `prune_fs_events_older_than`: chunked DELETE so a single
        // unbounded statement can't grab a large lock-list and stall live
        // writers. Only terminal-status rows are eligible — never touch
        // `pending`/`processing`, those are real work.
        //
        // `ORDER BY created_at, id` is required so the optimiser pins the
        // delete to `idx_retention (status, created_at)` and each 5000-row
        // chunk walks the index head-first instead of scanning the table.
        //
        // Cutoff is on `created_at`, NOT a real "finished_at" column —
        // schema has no such field. Implication: tasks that sit pending
        // for >N days and then transition to terminal in one batch (e.g.
        // a server restart that processes a long backlog) get pruned on
        // the very next sweep, losing post-mortem visibility. For alpha
        // single-user this is acceptable; if/when this bites, the fix is
        // a `finished_at TIMESTAMP NULL` column updated in complete/fail
        // — a forward-only schema change under alpha's fresh-redeploy
        // policy.
        const CHUNK: u64 = 5000;
        let mut total = 0u64;
        loop {
            let r = sqlx::query(
                r#"DELETE FROM veda_outbox
                   WHERE status IN ('completed','dead') AND created_at < ?
                   ORDER BY created_at, id
                   LIMIT 5000"#,
            )
            .bind(cutoff.naive_utc())
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

    async fn has_pending_event(
        &self,
        event_type: OutboxEventType,
        workspace_id: &str,
        payload_key: &str,
        payload_value: &str,
    ) -> Result<bool> {
        let et = db_enum_str(&event_type);
        let json_path = format!("$.{payload_key}");
        // Dedup against `pending` only — not `processing`. The original
        // `IN ('pending','processing')` swallowed updates that arrived
        // while a task held the snapshot, e.g. for DirSummarySync:
        // worker snapshots children at T1; a child SummarySync completes
        // at T2; the consequent enqueue_dedup is silently skipped; T1's
        // aggregate (missing T2's contribution) becomes the persisted
        // summary, and no future event ever re-aggregates. Same race
        // shape exists for ChunkSync (in-flight embed + new write =
        // dropped re-embed). Letting a fresh pending row coexist with
        // an in-flight row means worst-case we run one redundant pass
        // when racing; correctness wins over efficiency.
        let row: Option<(i64,)> = sqlx::query_as(
            r#"SELECT COUNT(*) FROM veda_outbox
               WHERE event_type = ? AND workspace_id = ? AND status = 'pending'
                 AND JSON_UNQUOTE(JSON_EXTRACT(payload, ?)) = ?"#,
        )
        .bind(et)
        .bind(workspace_id)
        .bind(&json_path)
        .bind(payload_value)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(row.map(|r| r.0 > 0).unwrap_or(false))
    }
}


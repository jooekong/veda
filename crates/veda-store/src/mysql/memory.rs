use super::conn::insert_outbox_conn;
use super::*;
use veda_core::store::MemoryStore;
use veda_types::{
    Memory, MemoryInsert, MemoryKind, MemoryPatch, MemoryScopeFilter, MemoryScopeType, NewMemory,
    PersonProfile, Principal, PrincipalKind, PrincipalSource,
};

/// MemorySync heal task, enqueued in the SAME transaction as the memory
/// write (the fs write-path invariant: data change and its sync task
/// commit together, so no crash window can leave the vector index silently
/// stale). The worker replay is idempotent — the synchronous Milvus write
/// that follows in the service is just latency optimization.
fn memory_sync_event(
    scope_type: MemoryScopeType,
    scope_id: &str,
    memory_id: i64,
    op: &str,
) -> OutboxEvent {
    OutboxEvent {
        id: 0,
        // Informational partition label; personal-domain rows have no
        // caller workspace at this layer, the scope id serves both.
        workspace_id: scope_id.to_string(),
        event_type: OutboxEventType::MemorySync,
        payload: serde_json::json!({
            "memory_id": memory_id,
            "op": op,
            "scope_type": scope_type.as_str(),
            "scope_id": scope_id,
        }),
        status: OutboxStatus::Pending,
        retry_count: 0,
        max_retries: 3,
        available_at: Utc::now(),
        lease_until: None,
        created_at: Utc::now(),
    }
}

const MEMORY_COLS: &str = "id, scope_type, scope_id, origin_workspace_id, topic, kind, content, \
     content_hash, source_ref, expires_at, last_used_at, created_by, created_at, updated_by, updated_at";

fn row_to_memory(row: &sqlx::mysql::MySqlRow) -> Result<Memory> {
    let st: String = row.try_get("scope_type").map_err(storage_err)?;
    let kind: String = row.try_get("kind").map_err(storage_err)?;
    let source_ref: Option<Json<serde_json::Value>> =
        row.try_get("source_ref").map_err(storage_err)?;
    Ok(Memory {
        id: row.try_get("id").map_err(storage_err)?,
        scope_type: db_enum("memory_scope_type", &st)?,
        scope_id: row.try_get("scope_id").map_err(storage_err)?,
        origin_workspace_id: row.try_get("origin_workspace_id").map_err(storage_err)?,
        topic: row.try_get("topic").map_err(storage_err)?,
        kind: db_enum::<MemoryKind>("memory_kind", &kind)?,
        content: row.try_get("content").map_err(storage_err)?,
        content_hash: row.try_get("content_hash").map_err(storage_err)?,
        source_ref: source_ref.map(|Json(v)| v),
        expires_at: row.try_get("expires_at").map_err(storage_err)?,
        last_used_at: row.try_get("last_used_at").map_err(storage_err)?,
        created_by: row.try_get("created_by").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_by: row.try_get("updated_by").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// SQL fragment for "row lies in one of the caller's writable domains".
/// `allowed` is 1-2 entries (team workspace / own principal).
fn allowed_expr(allowed: &[(MemoryScopeType, String)]) -> String {
    let parts: Vec<&str> = allowed
        .iter()
        .map(|_| "(scope_type = ? AND scope_id = ?)")
        .collect();
    format!("({})", parts.join(" OR "))
}

/// The read-side scope filter (single primitive, design §4.1). Literal
/// 'workspace'/'principal'/'dept' strings are constants, not caller input.
fn filter_expr(filter: &MemoryScopeFilter) -> &'static str {
    match filter {
        MemoryScopeFilter::Scope { .. } => "(scope_type = ? AND scope_id = ?)",
        MemoryScopeFilter::Context { dept_id: None, .. } => {
            "((scope_type = 'workspace' AND scope_id = ?) \
              OR (scope_type = 'principal' AND scope_id = ? \
                  AND (origin_workspace_id IS NULL OR origin_workspace_id = ?)))"
        }
        MemoryScopeFilter::Context { dept_id: Some(_), .. } => {
            "((scope_type = 'workspace' AND scope_id = ?) \
              OR (scope_type = 'principal' AND scope_id = ? \
                  AND (origin_workspace_id IS NULL OR origin_workspace_id = ?)) \
              OR (scope_type = 'dept' AND scope_id = ?))"
        }
    }
}

fn bind_filter<'q>(
    q: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    filter: &'q MemoryScopeFilter,
) -> sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    match filter {
        MemoryScopeFilter::Scope {
            scope_type,
            scope_id,
        } => q.bind(scope_type.as_str()).bind(scope_id),
        MemoryScopeFilter::Context {
            workspace_id,
            principal_id,
            dept_id,
        } => {
            let q = q.bind(workspace_id).bind(principal_id).bind(workspace_id);
            match dept_id {
                Some(d) => q.bind(d),
                None => q,
            }
        }
    }
}

#[async_trait]
impl MemoryStore for MysqlStore {
    async fn insert_memory(&self, mem: &NewMemory) -> Result<MemoryInsert> {
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        let res = sqlx::query(
            r#"INSERT INTO veda_memories
               (scope_type, scope_id, origin_workspace_id, topic, kind, content,
                content_hash, source_ref, expires_at, created_by, updated_by)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(mem.scope_type.as_str())
        .bind(&mem.scope_id)
        .bind(&mem.origin_workspace_id)
        .bind(&mem.topic)
        .bind(mem.kind.as_str())
        .bind(&mem.content)
        .bind(&mem.content_hash)
        .bind(mem.source_ref.clone().map(Json))
        .bind(mem.expires_at.map(|t| t.naive_utc()))
        .bind(&mem.created_by)
        .bind(&mem.created_by)
        .execute(&mut *tx)
        .await;
        match res {
            Ok(r) => {
                let id = r.last_insert_id() as i64;
                insert_outbox_conn(
                    &mut *tx,
                    &memory_sync_event(mem.scope_type, &mem.scope_id, id, "upsert"),
                )
                .await?;
                tx.commit().await.map_err(storage_err)?;
                let row = sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM veda_memories WHERE id = ?"
                ))
                .bind(id)
                .fetch_one(&self.pool)
                .await
                .map_err(storage_err)?;
                Ok(MemoryInsert::Inserted(row_to_memory(&row)?))
            }
            Err(e) if is_mysql_duplicate(&e) => {
                drop(tx);
                let row = sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM veda_memories \
                     WHERE scope_type = ? AND scope_id = ? AND content_hash = ?"
                ))
                .bind(mem.scope_type.as_str())
                .bind(&mem.scope_id)
                .bind(&mem.content_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(storage_err)?;
                match row {
                    Some(r) => {
                        let existing = row_to_memory(&r)?;
                        // A duplicate save is how a failed earlier save gets
                        // retried — persist a heal task so its vector cannot
                        // stay missing even if the service's sync write
                        // fails again.
                        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
                        insert_outbox_conn(
                            &mut *conn,
                            &memory_sync_event(
                                existing.scope_type,
                                &existing.scope_id,
                                existing.id,
                                "upsert",
                            ),
                        )
                        .await?;
                        Ok(MemoryInsert::Duplicate(existing))
                    }
                    // Duplicate row deleted between our INSERT and SELECT —
                    // rare race; caller retries the whole save.
                    None => Err(VedaError::Storage(
                        "duplicate memory vanished mid-insert, retry".into(),
                    )),
                }
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn update_memory(
        &self,
        id: i64,
        allowed: &[(MemoryScopeType, String)],
        patch: &MemoryPatch,
        updated_by: &str,
    ) -> Result<Memory> {
        if allowed.is_empty() {
            return Err(VedaError::NotFound(format!("memory {id}")));
        }
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        // Lock + scope-verify first: rows_affected can't distinguish
        // "no such row in your domains" from "update changed nothing"
        // (MySQL reports changed, not matched), and the heal task below
        // must only be enqueued for a row that really is ours.
        let lock_sql = format!(
            "SELECT scope_type, scope_id FROM veda_memories WHERE id = ? AND {} FOR UPDATE",
            allowed_expr(allowed)
        );
        let mut q = sqlx::query(&lock_sql).bind(id);
        for (st, sid) in allowed {
            q = q.bind(st.as_str()).bind(sid);
        }
        let Some(row) = q.fetch_optional(&mut *tx).await.map_err(storage_err)? else {
            return Err(VedaError::NotFound(format!("memory {id}")));
        };
        let st_str: String = row.try_get("scope_type").map_err(storage_err)?;
        let scope_type: MemoryScopeType = db_enum("memory_scope_type", &st_str)?;
        let scope_id: String = row.try_get("scope_id").map_err(storage_err)?;

        // A round-tripped unchanged scope is NOT a move — treating it as
        // one would wrongly clear origin_workspace_id (a pinned personal
        // note would go portable) and churn the vector row.
        let scope_move = patch
            .scope
            .as_ref()
            .filter(|(st, sid)| *st != scope_type || sid != &scope_id);
        let mut sets: Vec<&str> = Vec::new();
        if patch.content.is_some() {
            sets.push("content = ?");
            sets.push("content_hash = ?");
        }
        if patch.topic.is_some() {
            sets.push("topic = ?");
        }
        if patch.source_ref.is_some() {
            sets.push("source_ref = ?");
        }
        if patch.expires_at.is_some() {
            sets.push("expires_at = ?");
        }
        if scope_move.is_some() {
            // Scope move relocates the row in place; only personal rows
            // carry an origin, so it clears on any move.
            sets.push("scope_type = ?");
            sets.push("scope_id = ?");
            sets.push("origin_workspace_id = NULL");
        }
        sets.push("updated_by = ?");
        let sql = format!("UPDATE veda_memories SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(c) = &patch.content {
            let hash = patch
                .content_hash
                .as_ref()
                .ok_or_else(|| VedaError::Internal("content without content_hash".into()))?;
            q = q.bind(c).bind(hash);
        }
        if let Some(t) = &patch.topic {
            q = q.bind(t);
        }
        if let Some(s) = &patch.source_ref {
            q = q.bind(Json(s.clone()));
        }
        if let Some(e) = patch.expires_at {
            q = q.bind(e.naive_utc());
        }
        if let Some((st, sid)) = scope_move {
            q = q.bind(st.as_str()).bind(sid);
        }
        q = q.bind(updated_by).bind(id);
        match q.execute(&mut *tx).await {
            Ok(_) => {}
            Err(e) if is_mysql_duplicate(&e) => {
                return Err(VedaError::AlreadyExists(
                    "an identical memory already exists in the target scope".into(),
                ))
            }
            Err(e) => return Err(storage_err(e)),
        }
        // Content or scope changed → the vector row must follow. The event
        // carries the POST-update scope: the worker rereads under the
        // event's scope filter, so an old-scope event after a move would
        // miss the row and leave stale Milvus scalars (M3a R6).
        if patch.content.is_some() || scope_move.is_some() {
            let (sync_type, sync_id) = match scope_move {
                Some((st, sid)) => (*st, sid.clone()),
                None => (scope_type, scope_id.clone()),
            };
            insert_outbox_conn(
                &mut *tx,
                &memory_sync_event(sync_type, &sync_id, id, "upsert"),
            )
            .await?;
        }
        tx.commit().await.map_err(storage_err)?;

        let sql = format!("SELECT {MEMORY_COLS} FROM veda_memories WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(storage_err)?;
        row_to_memory(&row)
    }

    async fn delete_memory(
        &self,
        id: i64,
        allowed: &[(MemoryScopeType, String)],
    ) -> Result<bool> {
        if allowed.is_empty() {
            return Ok(false);
        }
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        let sql = format!(
            "DELETE FROM veda_memories WHERE id = ? AND {}",
            allowed_expr(allowed)
        );
        let mut q = sqlx::query(&sql).bind(id);
        for (st, sid) in allowed {
            q = q.bind(st.as_str()).bind(sid);
        }
        let res = q.execute(&mut *tx).await.map_err(storage_err)?;
        let deleted = res.rows_affected() > 0;
        if deleted {
            // Durable delete task: also supersedes any in-flight upsert heal
            // that read the row before this delete — the later delete task
            // clears whatever that replay writes back. Scope fields are
            // irrelevant for a vector delete-by-pk; the first allowed domain
            // serves as the informational partition label.
            let (label_type, label_id) = &allowed[0];
            insert_outbox_conn(
                &mut *tx,
                &memory_sync_event(*label_type, label_id, id, "delete"),
            )
            .await?;
        }
        tx.commit().await.map_err(storage_err)?;
        Ok(deleted)
    }

    async fn get_memories_by_ids(
        &self,
        ids: &[i64],
        filter: &MemoryScopeFilter,
    ) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM veda_memories \
             WHERE id IN ({}) AND {} \
               AND (expires_at IS NULL OR expires_at > NOW())",
            placeholders(ids.len()),
            filter_expr(filter)
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        q = bind_filter(q, filter);
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn touch_memories(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // `updated_at = updated_at` pins the audit timestamp against the
        // column's ON UPDATE CURRENT_TIMESTAMP — retrieval is not an edit.
        let sql = format!(
            "UPDATE veda_memories SET last_used_at = NOW(), updated_at = updated_at \
             WHERE id IN ({})",
            placeholders(ids.len())
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await.map_err(storage_err)?;
        Ok(())
    }

    async fn get_principal_by_identity(
        &self,
        source: PrincipalSource,
        external_id: &str,
    ) -> Result<Option<Principal>> {
        let row = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM veda_principal_identities i \
             JOIN veda_principals p ON p.id = i.principal_id \
             WHERE i.source = ? AND i.external_id = ?"
        ))
        .bind(source.as_str())
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.as_ref().map(row_to_principal).transpose()
    }

    async fn ensure_principal_for_identity(
        &self,
        source: PrincipalSource,
        external_id: &str,
        kind: PrincipalKind,
        profile: Option<&PersonProfile>,
    ) -> Result<Principal> {
        if let Some(p) = self.get_principal_by_identity(source, external_id).await? {
            return match profile {
                Some(prof) => self.apply_profile(p, prof).await,
                None => Ok(p),
            };
        }
        // New identity. A profile naming an already-known emp_no attaches
        // the identity to that principal — the cross-entrance merge.
        if let Some(prof) = profile {
            if let Some(owner) = self.get_principal_by_emp_no(&prof.emp_no).await? {
                self.insert_identity(source, external_id, &owner.id).await?;
                return self.apply_profile(owner, prof).await;
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO veda_principals \
             (id, kind, emp_no, display_name, dept_id, dept_name, profile_synced_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(kind.as_str())
        .bind(profile.map(|p| p.emp_no.as_str()))
        .bind(profile.and_then(|p| p.display_name.as_deref()))
        .bind(profile.and_then(|p| p.dept_id.as_deref()))
        .bind(profile.and_then(|p| p.dept_name.as_deref()))
        .bind(profile.map(|_| Utc::now().naive_utc()))
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => {
                self.insert_identity(source, external_id, &id).await?;
            }
            // Lost an emp_no race: another entrance created the person
            // between our lookup and insert — attach to the winner.
            Err(e) if is_mysql_duplicate(&e) => {
                if let Some(prof) = profile {
                    if let Some(owner) = self.get_principal_by_emp_no(&prof.emp_no).await? {
                        self.insert_identity(source, external_id, &owner.id).await?;
                    }
                }
            }
            Err(e) => return Err(storage_err(e)),
        }
        self.get_principal_by_identity(source, external_id)
            .await?
            .ok_or_else(|| VedaError::Storage("principal vanished after ensure".into()))
    }
}

const PRINCIPAL_COLS: &str = "p.id, p.kind, p.emp_no, p.display_name, p.dept_id, p.dept_name, \
     p.profile_synced_at, p.created_at";

fn row_to_principal(r: &sqlx::mysql::MySqlRow) -> Result<Principal> {
    let k: String = r.try_get("kind").map_err(storage_err)?;
    Ok(Principal {
        id: r.try_get("id").map_err(storage_err)?,
        kind: db_enum("principal_kind", &k)?,
        emp_no: r.try_get("emp_no").map_err(storage_err)?,
        display_name: r.try_get("display_name").map_err(storage_err)?,
        dept_id: r.try_get("dept_id").map_err(storage_err)?,
        dept_name: r.try_get("dept_name").map_err(storage_err)?,
        profile_synced_at: r.try_get("profile_synced_at").map_err(storage_err)?,
        created_at: r.try_get("created_at").map_err(storage_err)?,
    })
}

impl MysqlStore {
    async fn get_principal_by_emp_no(&self, emp_no: &str) -> Result<Option<Principal>> {
        let row = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM veda_principals p WHERE p.emp_no = ?"
        ))
        .bind(emp_no)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.as_ref().map(row_to_principal).transpose()
    }

    /// Idempotent identity insert: a duplicate means a racer attached this
    /// identity first — its binding wins, per the no-repoint rule.
    async fn insert_identity(
        &self,
        source: PrincipalSource,
        external_id: &str,
        principal_id: &str,
    ) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO veda_principal_identities (source, external_id, principal_id) \
             VALUES (?, ?, ?)",
        )
        .bind(source.as_str())
        .bind(external_id)
        .bind(principal_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_mysql_duplicate(&e) => Ok(()),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Refresh directory-derived fields. dept/name/synced_at always follow
    /// the directory (调岗 takes effect here); emp_no only fills in when
    /// free — a taken emp_no keeps the principals split with a warn, never
    /// repoints (M3a §1.2: merges are in-place backfills only).
    async fn apply_profile(&self, p: Principal, prof: &PersonProfile) -> Result<Principal> {
        sqlx::query(
            "UPDATE veda_principals SET display_name = ?, dept_id = ?, dept_name = ?, \
             profile_synced_at = ? WHERE id = ?",
        )
        .bind(prof.display_name.as_deref())
        .bind(prof.dept_id.as_deref())
        .bind(prof.dept_name.as_deref())
        .bind(Utc::now().naive_utc())
        .bind(&p.id)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        if p.emp_no.is_none() {
            let res = sqlx::query(
                "UPDATE veda_principals SET emp_no = ? WHERE id = ? AND emp_no IS NULL",
            )
            .bind(&prof.emp_no)
            .bind(&p.id)
            .execute(&self.pool)
            .await;
            match res {
                Ok(_) => {}
                Err(e) if is_mysql_duplicate(&e) => {
                    tracing::warn!(
                        principal = %p.id,
                        emp_no = %prof.emp_no,
                        "emp_no already owned by another principal; keeping identities split"
                    );
                }
                Err(e) => return Err(storage_err(e)),
            }
        }
        let row = sqlx::query(&format!(
            "SELECT {PRINCIPAL_COLS} FROM veda_principals p WHERE p.id = ?"
        ))
        .bind(&p.id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;
        row_to_principal(&row)
    }
}

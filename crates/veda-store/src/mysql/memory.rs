use super::*;
use veda_core::store::MemoryStore;
use veda_types::{
    Memory, MemoryInsert, MemoryKind, MemoryPatch, MemoryScopeFilter, MemoryScopeType, NewMemory,
    Principal, PrincipalKind, PrincipalSource,
};

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
/// 'workspace'/'principal' strings are constants, not caller input.
fn filter_expr(filter: &MemoryScopeFilter) -> &'static str {
    match filter {
        MemoryScopeFilter::Scope { .. } => "(scope_type = ? AND scope_id = ?)",
        MemoryScopeFilter::Context { .. } => {
            "((scope_type = 'workspace' AND scope_id = ?) \
              OR (scope_type = 'principal' AND scope_id = ? \
                  AND (origin_workspace_id IS NULL OR origin_workspace_id = ?)))"
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
        } => q.bind(workspace_id).bind(principal_id).bind(workspace_id),
    }
}

#[async_trait]
impl MemoryStore for MysqlStore {
    async fn insert_memory(&self, mem: &NewMemory) -> Result<MemoryInsert> {
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
        .execute(&self.pool)
        .await;
        match res {
            Ok(r) => {
                let id = r.last_insert_id() as i64;
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
                    Some(r) => Ok(MemoryInsert::Duplicate(row_to_memory(&r)?)),
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
        sets.push("updated_by = ?");
        let sql = format!(
            "UPDATE veda_memories SET {} WHERE id = ? AND {}",
            sets.join(", "),
            allowed_expr(allowed)
        );
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
        q = q.bind(updated_by).bind(id);
        for (st, sid) in allowed {
            q = q.bind(st.as_str()).bind(sid);
        }
        let res = q.execute(&self.pool).await;
        match res {
            Ok(_) => {}
            Err(e) if is_mysql_duplicate(&e) => {
                return Err(VedaError::AlreadyExists(
                    "an identical memory already exists in this scope".into(),
                ))
            }
            Err(e) => return Err(storage_err(e)),
        }
        // rows_affected can be 0 for a no-change update (MySQL reports
        // changed rows, not matched), so verify by re-reading under the
        // same scope guard instead.
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM veda_memories WHERE id = ? AND {}",
            allowed_expr(allowed)
        );
        let mut q = sqlx::query(&sql).bind(id);
        for (st, sid) in allowed {
            q = q.bind(st.as_str()).bind(sid);
        }
        let row = q.fetch_optional(&self.pool).await.map_err(storage_err)?;
        match row {
            Some(r) => row_to_memory(&r),
            None => Err(VedaError::NotFound(format!("memory {id}"))),
        }
    }

    async fn delete_memory(
        &self,
        id: i64,
        allowed: &[(MemoryScopeType, String)],
    ) -> Result<bool> {
        if allowed.is_empty() {
            return Ok(false);
        }
        let sql = format!(
            "DELETE FROM veda_memories WHERE id = ? AND {}",
            allowed_expr(allowed)
        );
        let mut q = sqlx::query(&sql).bind(id);
        for (st, sid) in allowed {
            q = q.bind(st.as_str()).bind(sid);
        }
        let res = q.execute(&self.pool).await.map_err(storage_err)?;
        Ok(res.rows_affected() > 0)
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

    async fn ensure_principal(
        &self,
        source: PrincipalSource,
        external_id: &str,
        kind: PrincipalKind,
        display_name: Option<&str>,
    ) -> Result<Principal> {
        const COLS: &str = "id, kind, source, external_id, display_name, created_at";
        let get = |pool: MySqlPool| async move {
            let row = sqlx::query(&format!(
                "SELECT {COLS} FROM veda_principals WHERE source = ? AND external_id = ?"
            ))
            .bind(source.as_str())
            .bind(external_id)
            .fetch_optional(&pool)
            .await
            .map_err(storage_err)?;
            row.map(|r| -> Result<Principal> {
                let k: String = r.try_get("kind").map_err(storage_err)?;
                let s: String = r.try_get("source").map_err(storage_err)?;
                Ok(Principal {
                    id: r.try_get("id").map_err(storage_err)?,
                    kind: db_enum("principal_kind", &k)?,
                    source: db_enum("principal_source", &s)?,
                    external_id: r.try_get("external_id").map_err(storage_err)?,
                    display_name: r.try_get("display_name").map_err(storage_err)?,
                    created_at: r.try_get("created_at").map_err(storage_err)?,
                })
            })
            .transpose()
        };
        if let Some(p) = get(self.pool.clone()).await? {
            return Ok(p);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let res = sqlx::query(
            "INSERT INTO veda_principals (id, kind, source, external_id, display_name) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(kind.as_str())
        .bind(source.as_str())
        .bind(external_id)
        .bind(display_name)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => {}
            // Lost the first-sighting race — the winner's row is what we want.
            Err(e) if is_mysql_duplicate(&e) => {}
            Err(e) => return Err(storage_err(e)),
        }
        get(self.pool.clone())
            .await?
            .ok_or_else(|| VedaError::Storage("principal vanished after ensure".into()))
    }
}

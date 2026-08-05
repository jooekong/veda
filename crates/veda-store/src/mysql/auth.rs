use super::*;

// ── AuthStore ──────────────────────────────────────────

#[async_trait]
impl AuthStore for MysqlStore {
    async fn create_account(&self, account: &Account) -> Result<()> {
        let res = sqlx::query(
            r#"INSERT INTO veda_accounts (id, name, email, password_hash, app_id, status, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&account.id)
        .bind(&account.name)
        .bind(&account.email)
        .bind(&account.password_hash)
        .bind(&account.app_id)
        .bind(db_enum_str(&account.status))
        .bind(account.created_at.naive_utc())
        .bind(account.updated_at.naive_utc())
        .execute(&self.pool)
        .await;
        // UNIQUE(email) or UNIQUE(app_id) collision → MySQL 1062 → 409.
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_mysql_duplicate(&e) => Err(VedaError::AlreadyExists(
                "account email or app_id already exists".into(),
            )),
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn get_account(&self, id: &str) -> Result<Option<Account>> {
        let row = sqlx::query(
            r#"SELECT id, name, email, password_hash, app_id, status, created_at, updated_at
               FROM veda_accounts WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_account(&r)).transpose()
    }

    async fn get_account_by_email(&self, email: &str) -> Result<Option<Account>> {
        let row = sqlx::query(
            r#"SELECT id, name, email, password_hash, app_id, status, created_at, updated_at
               FROM veda_accounts WHERE email = ?"#,
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_account(&r)).transpose()
    }

    async fn get_account_by_app_id(&self, app_id: &str) -> Result<Option<Account>> {
        let row = sqlx::query(
            r#"SELECT id, name, email, password_hash, app_id, status, created_at, updated_at
               FROM veda_accounts WHERE app_id = ?"#,
        )
        .bind(app_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_account(&r)).transpose()
    }

    async fn claim_account(
        &self,
        id: &str,
        email: &str,
        password_hash: &str,
        name: Option<&str>,
    ) -> Result<()> {
        // `AND email IS NULL` makes claim idempotent + race-safe: a
        // racing second claim sees 0 rows affected (rather than
        // overwriting the first claim's email/password). COALESCE
        // keeps the existing name when caller passes None.
        let res = sqlx::query(
            r#"UPDATE veda_accounts
               SET email = ?,
                   password_hash = ?,
                   name = COALESCE(?, name),
                   updated_at = ?
               WHERE id = ? AND email IS NULL AND app_id IS NULL"#,
        )
        .bind(email)
        .bind(password_hash)
        .bind(name)
        .bind(Utc::now().naive_utc())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(translate_account_email_conflict)?;
        if res.rows_affected() == 0 {
            // Either the id doesn't exist, or the account was already
            // claimed (email IS NOT NULL). Map to Unauthorized so the
            // caller's vk_ being valid doesn't leak which condition
            // hit.
            return Err(VedaError::Unauthorized(
                "account is no longer anonymous".into(),
            ));
        }
        Ok(())
    }

    async fn create_anonymous_bundle(
        &self,
        account: &Account,
        api_key: &ApiKeyRecord,
        workspace: &Workspace,
        ws_key: &WorkspaceKey,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(storage_err)?;

        sqlx::query(
            r#"INSERT INTO veda_accounts (id, name, email, password_hash, app_id, status, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&account.id)
        .bind(&account.name)
        .bind(&account.email)
        .bind(&account.password_hash)
        .bind(&account.app_id)
        .bind(db_enum_str(&account.status))
        .bind(account.created_at.naive_utc())
        .bind(account.updated_at.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            r#"INSERT INTO veda_api_keys
               (id, account_id, name, key_hash, status, app_id, allowed_workspaces, expires_at, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&api_key.id)
        .bind(&api_key.account_id)
        .bind(&api_key.name)
        .bind(&api_key.key_hash)
        .bind(db_enum_str(&api_key.status))
        .bind(&api_key.app_id)
        .bind(api_key.allowed_workspaces.as_ref().map(Json))
        .bind(api_key.expires_at.map(|d| d.naive_utc()))
        .bind(api_key.created_at.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            r#"INSERT INTO veda_workspaces (id, account_id, name, status, kind, app_id, description, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&workspace.id)
        .bind(&workspace.account_id)
        .bind(&workspace.name)
        .bind(db_enum_str(&workspace.status))
        .bind(db_enum_str(&workspace.kind))
        .bind(&workspace.app_id)
        .bind(&workspace.description)
        .bind(workspace.created_at.naive_utc())
        .bind(workspace.updated_at.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            r#"INSERT INTO veda_workspace_keys (id, workspace_id, account_id, name, key_hash, permission, status, kind, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&ws_key.id)
        .bind(&ws_key.workspace_id)
        .bind(&ws_key.account_id)
        .bind(&ws_key.name)
        .bind(&ws_key.key_hash)
        .bind(db_enum_str(&ws_key.permission))
        .bind(db_enum_str(&ws_key.status))
        .bind(db_enum_str(&ws_key.kind))
        .bind(ws_key.created_at.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(storage_err)?;

        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn create_api_key(&self, key: &ApiKeyRecord) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO veda_api_keys
               (id, account_id, name, key_hash, status, app_id, allowed_workspaces, expires_at, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&key.id)
        .bind(&key.account_id)
        .bind(&key.name)
        .bind(&key.key_hash)
        .bind(db_enum_str(&key.status))
        .bind(&key.app_id)
        .bind(key.allowed_workspaces.as_ref().map(Json))
        .bind(key.expires_at.map(|d| d.naive_utc()))
        .bind(key.created_at.naive_utc())
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn get_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRecord>> {
        // JOIN veda_accounts so a suspended account's keys stop authorizing
        // immediately. Without this, AccountStatus::Suspended is dead weight.
        // expires_at filter: NULL = never expires; non-NULL = enforce.
        let row = sqlx::query(
            r#"SELECT k.id, k.account_id, k.name, k.key_hash, k.status,
                      k.app_id, k.allowed_workspaces, k.expires_at, k.created_at
               FROM veda_api_keys k
               INNER JOIN veda_accounts a ON a.id = k.account_id
               WHERE k.key_hash = ?
                 AND k.status = 'active'
                 AND a.status = 'active'
                 AND (k.expires_at IS NULL OR k.expires_at > NOW())"#,
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_api_key(&r)).transpose()
    }

    async fn get_api_key_by_id(&self, id: &str) -> Result<Option<ApiKeyRecord>> {
        let row = sqlx::query(
            r#"SELECT id, account_id, name, key_hash, status,
                      app_id, allowed_workspaces, expires_at, created_at
               FROM veda_api_keys WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_api_key(&r)).transpose()
    }

    async fn list_api_keys(&self, account_id: &str) -> Result<Vec<ApiKeyRecord>> {
        let rows = sqlx::query(
            r#"SELECT id, account_id, name, key_hash, status,
                      app_id, allowed_workspaces, expires_at, created_at
               FROM veda_api_keys WHERE account_id = ? ORDER BY created_at"#,
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter().map(|r| row_to_api_key(r)).collect()
    }

    async fn revoke_api_key(&self, id: &str) -> Result<()> {
        sqlx::query(r#"UPDATE veda_api_keys SET status = 'revoked' WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn create_workspace(&self, workspace: &Workspace) -> Result<()> {
        let res = sqlx::query(
            r#"INSERT INTO veda_workspaces (id, account_id, name, status, kind, app_id, description, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&workspace.id)
        .bind(&workspace.account_id)
        .bind(&workspace.name)
        .bind(db_enum_str(&workspace.status))
        .bind(db_enum_str(&workspace.kind))
        .bind(&workspace.app_id)
        .bind(&workspace.description)
        .bind(workspace.created_at.naive_utc())
        .bind(workspace.updated_at.naive_utc())
        .execute(&self.pool)
        .await;
        // UNIQUE(account_id, name) collision → 1062 → 409, mirroring
        // create_db_workspace so an fs workspace name clash returns a clean
        // 409 instead of an opaque 500 INTERNAL.
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_mysql_duplicate(&e) => {
                Err(VedaError::AlreadyExists("workspace name already exists".into()))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn set_workspace_creator(
        &self,
        workspace_id: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(r#"UPDATE veda_workspaces SET creator = ?, creator_name = ? WHERE id = ?"#)
            .bind(creator)
            .bind(creator_name)
            .bind(workspace_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn update_workspace(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            r#"UPDATE veda_workspaces SET name = ?, description = ?, updated_at = ? WHERE id = ?"#,
        )
        .bind(name)
        .bind(description)
        .bind(Utc::now().naive_utc())
        .bind(id)
        .execute(&self.pool)
        .await;
        // UNIQUE(account_id, name) collision on rename → 409, mirroring create.
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_mysql_duplicate(&e) => {
                Err(VedaError::AlreadyExists("workspace name already exists".into()))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn get_workspace_creator(
        &self,
        id: &str,
    ) -> Result<(Option<String>, Option<String>)> {
        let row = sqlx::query("SELECT creator, creator_name FROM veda_workspaces WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?;
        match row {
            Some(r) => Ok((
                r.try_get("creator").map_err(storage_err)?,
                r.try_get("creator_name").map_err(storage_err)?,
            )),
            None => Ok((None, None)),
        }
    }

    async fn list_app_workspaces(
        &self,
        account_id: &str,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Workspace, Option<String>, Option<String>)>, i64)> {
        let cols = "id, account_id, name, status, kind, app_id, description, created_at, updated_at, creator, creator_name";
        let (ob, od) = order_clause(order_by, order);
        let sql = format!(
            "SELECT {cols} FROM veda_workspaces \
             WHERE account_id = ? AND status = 'active' ORDER BY {ob} {od} LIMIT ? OFFSET ?"
        );
        let rows = sqlx::query(&sql)
            .bind(account_id)
            .bind(size as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM veda_workspaces WHERE account_id = ? AND status = 'active'",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;
        let items: Result<Vec<_>> = rows
            .iter()
            .map(|r| {
                let ws = row_to_workspace(r)?;
                let creator: Option<String> = r.try_get("creator").map_err(storage_err)?;
                let creator_name: Option<String> = r.try_get("creator_name").map_err(storage_err)?;
                Ok((ws, creator, creator_name))
            })
            .collect();
        Ok((items?, total))
    }

    async fn list_app_workspaces_for_accounts(
        &self,
        account_ids: &[String],
        keyword: Option<&str>,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Workspace, Option<String>, Option<String>)>, i64)> {
        if account_ids.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let cols = "id, account_id, name, status, kind, app_id, description, created_at, updated_at, creator, creator_name";
        let (ob, od) = order_clause(order_by, order);
        let ph = vec!["?"; account_ids.len()].join(",");
        // Optional case-insensitive keyword filter over name OR description
        // (ci collation). Bound param, so the value is escaped; `%`/`_` in the
        // keyword act as LIKE wildcards. A NULL description simply doesn't match
        // — the OR still lets a name hit through. Bound twice (name + desc).
        let kw_clause = if keyword.is_some() {
            " AND (name LIKE ? OR description LIKE ?)"
        } else {
            ""
        };
        let like = keyword.map(|k| format!("%{k}%"));
        let sql = format!(
            "SELECT {cols} FROM veda_workspaces \
             WHERE account_id IN ({ph}) AND status = 'active'{kw_clause} ORDER BY {ob} {od} LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query(&sql);
        for id in account_ids {
            q = q.bind(id);
        }
        if let Some(ref l) = like {
            // two placeholders: name LIKE ? OR description LIKE ?
            q = q.bind(l).bind(l);
        }
        let rows = q
            .bind(size as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        let count_sql = format!(
            "SELECT COUNT(*) FROM veda_workspaces WHERE account_id IN ({ph}) AND status = 'active'{kw_clause}"
        );
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
        for id in account_ids {
            cq = cq.bind(id);
        }
        if let Some(ref l) = like {
            cq = cq.bind(l).bind(l);
        }
        let total: i64 = cq.fetch_one(&self.pool).await.map_err(storage_err)?;
        let items: Result<Vec<_>> = rows
            .iter()
            .map(|r| {
                let ws = row_to_workspace(r)?;
                let creator: Option<String> = r.try_get("creator").map_err(storage_err)?;
                let creator_name: Option<String> = r.try_get("creator_name").map_err(storage_err)?;
                Ok((ws, creator, creator_name))
            })
            .collect();
        Ok((items?, total))
    }

    async fn create_db_workspace(
        &self,
        workspace: &Workspace,
        dataset: &Dataset,
    ) -> Result<()> {
        // MySQL duplicate-key (UNIQUE violation). veda_workspaces has
        // UNIQUE(account_id, name), veda_datasets has UNIQUE(workspace_id,
        // name). Map both to AlreadyExists so the route returns 409 instead
        // of an opaque 500.
        fn is_dup_key(e: &sqlx::Error) -> bool {
            matches!(e, sqlx::Error::Database(db) if db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|x| x.number() == 1062)
                .unwrap_or(false))
        }

        let mut tx = self.pool.begin().await.map_err(storage_err)?;

        if let Err(e) = sqlx::query(
            r#"INSERT INTO veda_workspaces (id, account_id, name, status, kind, app_id, description, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&workspace.id)
        .bind(&workspace.account_id)
        .bind(&workspace.name)
        .bind(db_enum_str(&workspace.status))
        .bind(db_enum_str(&workspace.kind))
        .bind(&workspace.app_id)
        .bind(&workspace.description)
        .bind(workspace.created_at.naive_utc())
        .bind(workspace.updated_at.naive_utc())
        .execute(&mut *tx)
        .await
        {
            if is_dup_key(&e) {
                return Err(VedaError::AlreadyExists(format!(
                    "workspace {}",
                    workspace.name
                )));
            }
            return Err(storage_err(e));
        }

        if let Err(e) = sqlx::query(
            r#"INSERT INTO veda_datasets (id, workspace_id, name, status, description, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&dataset.id)
        .bind(&dataset.workspace_id)
        .bind(&dataset.name)
        .bind(db_enum_str(&dataset.status))
        .bind(&dataset.description)
        .bind(dataset.created_at.naive_utc())
        .bind(dataset.updated_at.naive_utc())
        .execute(&mut *tx)
        .await
        {
            // tx drops here → the workspace insert rolls back too.
            if is_dup_key(&e) {
                return Err(VedaError::AlreadyExists(format!("dataset {}", dataset.name)));
            }
            return Err(storage_err(e));
        }

        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn get_workspace(&self, id: &str) -> Result<Option<Workspace>> {
        let row = sqlx::query(
            r#"SELECT id, account_id, name, status, kind, app_id, description, created_at, updated_at
               FROM veda_workspaces WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_workspace(&r)).transpose()
    }

    async fn list_workspaces(
        &self,
        account_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Workspace>, bool)> {
        // Fetch limit+1 to detect has_more without a separate COUNT query.
        let fetch_n = (limit as i64) + 1;
        let rows = match after {
            Some(cursor) => sqlx::query(
                r#"SELECT id, account_id, name, status, kind, app_id, description, created_at, updated_at
                   FROM veda_workspaces
                   WHERE account_id = ? AND status = 'active' AND id > ?
                   ORDER BY id LIMIT ?"#,
            )
            .bind(account_id)
            .bind(cursor)
            .bind(fetch_n),
            None => sqlx::query(
                r#"SELECT id, account_id, name, status, kind, app_id, description, created_at, updated_at
                   FROM veda_workspaces
                   WHERE account_id = ? AND status = 'active'
                   ORDER BY id LIMIT ?"#,
            )
            .bind(account_id)
            .bind(fetch_n),
        }
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let has_more = rows.len() > limit as usize;
        let items: Result<Vec<_>> = rows
            .iter()
            .take(limit as usize)
            .map(row_to_workspace)
            .collect();
        Ok((items?, has_more))
    }

    async fn list_active_workspace_ids(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT id FROM veda_workspaces WHERE status = 'active'"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn list_all_workspaces_with_counts(
        &self,
    ) -> Result<Vec<(Workspace, i64, i64, Option<String>, Option<String>)>> {
        // Correlated COUNT subqueries keep this a single round-trip over the
        // small control-plane tables (no Milvus / dentry scan here — fs byte
        // stats are fetched per-workspace by the handler). dataset/key counts
        // are scoped to active rows to match what the data plane can reach.
        let rows = sqlx::query(
            r#"SELECT w.id, w.account_id, w.name, w.status, w.kind, w.app_id,
                      w.description, w.created_at, w.updated_at,
                      w.creator, w.creator_name,
                      (SELECT COUNT(*) FROM veda_datasets d
                         WHERE d.workspace_id = w.id AND d.status = 'active') AS dataset_count,
                      (SELECT COUNT(*) FROM veda_workspace_keys k
                         WHERE k.workspace_id = w.id AND k.status = 'active') AS key_count
               FROM veda_workspaces w
               WHERE w.status = 'active'
               ORDER BY w.created_at DESC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter()
            .map(|r| {
                let ws = row_to_workspace(r)?;
                let dataset_count: i64 = r.try_get("dataset_count").map_err(storage_err)?;
                let key_count: i64 = r.try_get("key_count").map_err(storage_err)?;
                let creator: Option<String> = r.try_get("creator").map_err(storage_err)?;
                let creator_name: Option<String> = r.try_get("creator_name").map_err(storage_err)?;
                Ok((ws, dataset_count, key_count, creator, creator_name))
            })
            .collect()
    }

    async fn delete_workspace(&self, id: &str) -> Result<()> {
        // Archive the workspace AND revoke its wk_ keys in one transaction.
        // wk_ auth no longer checks workspace.status (see
        // get_workspace_key_by_hash), so revoking the keys is what actually
        // stops the data plane. Atomic: leaving the workspace active with
        // keys revoked (outage) or archived with keys live (auth bypass)
        // are both unacceptable — both updates commit or neither does.
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        sqlx::query(r#"UPDATE veda_workspace_keys SET status = 'revoked' WHERE workspace_id = ?"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(storage_err)?;
        sqlx::query(r#"UPDATE veda_workspaces SET status = 'archived' WHERE id = ?"#)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(storage_err)?;
        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn hard_delete_workspace(&self, id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_workspaces WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn create_dataset(&self, dataset: &Dataset) -> Result<()> {
        // UNIQUE (workspace_id, name) collisions surface as MySQL error 1062.
        // Map to `AlreadyExists` so the route layer can return 409 cleanly
        // (instead of an opaque 500 from a generic Storage error).
        let res = sqlx::query(
            r#"INSERT INTO veda_datasets (id, workspace_id, name, status, description, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&dataset.id)
        .bind(&dataset.workspace_id)
        .bind(&dataset.name)
        .bind(db_enum_str(&dataset.status))
        .bind(&dataset.description)
        .bind(dataset.created_at.naive_utc())
        .bind(dataset.updated_at.naive_utc())
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db_err))
                if db_err
                    .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                    .map(|e| e.number() == 1062)
                    .unwrap_or(false) =>
            {
                Err(VedaError::AlreadyExists(format!("dataset {}", dataset.name)))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn list_active_datasets(
        &self,
        workspace_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Dataset>, bool)> {
        // Fetch limit+1 to detect has_more without a separate COUNT query.
        let fetch_n = (limit as i64) + 1;
        let rows = match after {
            Some(cursor) => sqlx::query(
                r#"SELECT id, workspace_id, name, status, description, created_at, updated_at
                   FROM veda_datasets
                   WHERE workspace_id = ? AND status = 'active' AND id > ?
                   ORDER BY id LIMIT ?"#,
            )
            .bind(workspace_id)
            .bind(cursor)
            .bind(fetch_n),
            None => sqlx::query(
                r#"SELECT id, workspace_id, name, status, description, created_at, updated_at
                   FROM veda_datasets
                   WHERE workspace_id = ? AND status = 'active'
                   ORDER BY id LIMIT ?"#,
            )
            .bind(workspace_id)
            .bind(fetch_n),
        }
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let has_more = rows.len() > limit as usize;
        let items: Result<Vec<_>> = rows.iter().take(limit as usize).map(row_to_dataset).collect();
        Ok((items?, has_more))
    }

    async fn get_active_dataset_by_name(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<Dataset>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, name, status, description, created_at, updated_at
               FROM veda_datasets
               WHERE workspace_id = ? AND name = ? AND status = 'active'"#,
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_dataset(&r)).transpose()
    }

    async fn archive_dataset(&self, workspace_id: &str, name: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"UPDATE veda_datasets SET status = 'archived'
               WHERE workspace_id = ? AND name = ? AND status = 'active'"#,
        )
        .bind(workspace_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn hard_delete_datasets_for_workspace(&self, workspace_id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_datasets WHERE workspace_id = ?"#)
            .bind(workspace_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn create_workspace_key(&self, key: &WorkspaceKey) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO veda_workspace_keys (id, workspace_id, account_id, name, key_hash, permission, status, kind, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&key.id)
        .bind(&key.workspace_id)
        .bind(&key.account_id)
        .bind(&key.name)
        .bind(&key.key_hash)
        .bind(db_enum_str(&key.permission))
        .bind(db_enum_str(&key.status))
        .bind(db_enum_str(&key.kind))
        .bind(key.created_at.naive_utc())
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn get_workspace_key_by_hash(&self, key_hash: &str) -> Result<Option<WorkspaceKey>> {
        // JOIN account so a suspended account's keys stop authorizing
        // immediately (account suspend has no veda write path to cascade
        // through, so this read-time check is how it takes effect).
        // workspace.status is NOT checked — archiving a workspace cascades a
        // key revoke in `delete_workspace`. `kind`/`account_id` are
        // denormalized onto the key, so this is a single-table lookup + one
        // PK JOIN, replacing the old 3-table JOIN + a second get_workspace.
        let row = sqlx::query(
            r#"SELECT k.id, k.workspace_id, k.account_id, k.name, k.key_hash,
                      k.permission, k.status, k.kind, k.created_at
               FROM veda_workspace_keys k
               INNER JOIN veda_accounts a ON a.id = k.account_id
               WHERE k.key_hash = ? AND k.status = 'active' AND a.status = 'active'"#,
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_workspace_key(&r)).transpose()
    }

    async fn list_workspace_keys(&self, workspace_id: &str) -> Result<Vec<WorkspaceKey>> {
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, account_id, name, key_hash, permission, status, kind, created_at
               FROM veda_workspace_keys WHERE workspace_id = ? ORDER BY created_at"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter().map(|r| row_to_workspace_key(r)).collect()
    }

    async fn create_app_workspace_key(
        &self,
        key: &WorkspaceKey,
        token: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO veda_workspace_keys (id, workspace_id, account_id, name, key_hash, permission, status, kind, created_at, token, creator, creator_name)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&key.id)
        .bind(&key.workspace_id)
        .bind(&key.account_id)
        .bind(&key.name)
        .bind(&key.key_hash)
        .bind(db_enum_str(&key.permission))
        .bind(db_enum_str(&key.status))
        .bind(db_enum_str(&key.kind))
        .bind(key.created_at.naive_utc())
        .bind(token)
        .bind(creator)
        .bind(creator_name)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn list_app_workspace_keys(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<(WorkspaceKey, Option<String>, Option<String>, Option<String>)>> {
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, account_id, name, key_hash, permission, status, kind, created_at, token, creator, creator_name
               FROM veda_workspace_keys WHERE workspace_id = ? ORDER BY created_at"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter()
            .map(|r| {
                let key = row_to_workspace_key(r)?;
                let token: Option<String> = r.try_get("token").map_err(storage_err)?;
                let creator: Option<String> = r.try_get("creator").map_err(storage_err)?;
                let creator_name: Option<String> = r.try_get("creator_name").map_err(storage_err)?;
                Ok((key, token, creator, creator_name))
            })
            .collect()
    }

    async fn get_workspace_key_token(
        &self,
        key_id: &str,
        workspace_id: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            r#"SELECT token FROM veda_workspace_keys WHERE id = ? AND workspace_id = ?"#,
        )
        .bind(key_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        match row {
            Some(r) => Ok(r.try_get("token").map_err(storage_err)?),
            None => Ok(None),
        }
    }

    async fn set_dataset_creator(
        &self,
        dataset_id: &str,
        creator: Option<&str>,
        creator_name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(r#"UPDATE veda_datasets SET creator = ?, creator_name = ? WHERE id = ?"#)
            .bind(creator)
            .bind(creator_name)
            .bind(dataset_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list_app_datasets(
        &self,
        workspace_id: &str,
        offset: u32,
        size: u32,
        order_by: &str,
        order: &str,
    ) -> Result<(Vec<(Dataset, Option<String>, Option<String>)>, i64)> {
        let cols = "id, workspace_id, name, status, description, created_at, updated_at, creator, creator_name";
        let (ob, od) = order_clause(order_by, order);
        let sql = format!(
            "SELECT {cols} FROM veda_datasets \
             WHERE workspace_id = ? AND status = 'active' ORDER BY {ob} {od} LIMIT ? OFFSET ?"
        );
        let rows = sqlx::query(&sql)
            .bind(workspace_id)
            .bind(size as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM veda_datasets WHERE workspace_id = ? AND status = 'active'",
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage_err)?;
        let items: Result<Vec<_>> = rows
            .iter()
            .map(|r| {
                let ds = row_to_dataset(r)?;
                let creator: Option<String> = r.try_get("creator").map_err(storage_err)?;
                let creator_name: Option<String> = r.try_get("creator_name").map_err(storage_err)?;
                Ok((ds, creator, creator_name))
            })
            .collect();
        Ok((items?, total))
    }

    async fn revoke_workspace_key(&self, id: &str, workspace_id: &str) -> Result<()> {
        sqlx::query(
            r#"UPDATE veda_workspace_keys SET status = 'revoked' WHERE id = ? AND workspace_id = ?"#,
        )
        .bind(id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }
}


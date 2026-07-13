//! Platform-side access to `veda_tunnel_bots` — the WeCom bot table SHARED
//! with veda-tunnel (see `crates/veda-tunnel/src/store.rs`, which owns the
//! DDL; the CREATE here is a byte-for-byte copy so whichever process starts
//! first bootstraps it — keep them in sync).
//!
//! The AI Workbench talks only to veda-server, and veda-tunnel talks only to
//! MySQL: platform CRUD lands here as plain row writes, and tunnel's 30s
//! store poll converges live WeCom connections onto the table. No RPC
//! between the two processes.
//!
//! Uses its own tiny pool (2 conns) on the same database_url as the main
//! store — tunnel-bot admin traffic never contends with the auth pool.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::Row;
use veda_types::VedaError;

pub struct TunnelBotStore {
    pool: MySqlPool,
}

/// One platform-visible bot row. `secret` is deliberately absent — it is
/// write-only through this surface.
#[derive(Debug, Clone)]
pub struct TunnelBotRow {
    pub bot_id: String,
    pub name: String,
    pub veda_key: String,
    pub workspace: String,
    pub project: Option<String>,
    pub mode: String,
    pub search_limit: i32,
    pub key_id: Option<String>,
    pub creator: Option<String>,
    pub creator_name: Option<String>,
    pub conn_state: String,
    pub conn_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fields for a platform create. `workspace` = platform tenant code,
/// `project` = veda project (workspace) id — both stamped from the URL path,
/// never from the body.
pub struct NewTunnelBot {
    pub bot_id: String,
    pub name: String,
    pub secret: String,
    pub veda_key: String,
    pub workspace: String,
    pub project: String,
    pub mode: String,
    pub search_limit: i32,
    pub key_id: String,
    pub creator: Option<String>,
    pub creator_name: Option<String>,
}

/// Patchable fields (PATCH semantics: `None` = keep).
#[derive(Default)]
pub struct TunnelBotPatch {
    pub name: Option<String>,
    pub secret: Option<String>,
    pub mode: Option<String>,
    pub search_limit: Option<i32>,
}

impl TunnelBotStore {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .context("connect tunnel-bots MySQL")?;
        let store = Self { pool };
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Copy of veda-tunnel's bootstrap DDL + column migration (owner:
    /// veda-tunnel/store.rs). Both sides run the same idempotent
    /// CREATE + ALTER so deploy order doesn't matter — a veda-server carrying
    /// this API can land before or after the tunnel that owns the table.
    async fn ensure_schema(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS veda_tunnel_bots (
                bot_id       VARCHAR(128) NOT NULL PRIMARY KEY,
                name         VARCHAR(128) NOT NULL,
                secret       VARCHAR(256) NOT NULL,
                veda_key     VARCHAR(128) NOT NULL,
                workspace    VARCHAR(128) NOT NULL,
                project      VARCHAR(128) NULL,
                mode         VARCHAR(32)  NOT NULL DEFAULT 'hybrid',
                search_limit INT          NOT NULL DEFAULT 8,
                key_id       VARCHAR(64)  NULL,
                creator      VARCHAR(128) NULL,
                creator_name VARCHAR(128) NULL,
                conn_state   VARCHAR(16)  NOT NULL DEFAULT 'unknown',
                conn_updated_at TIMESTAMP NULL,
                created_at   TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at   TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
                UNIQUE KEY uk_name (name),
                KEY idx_project (project)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
            "#,
        )
        .execute(&self.pool)
        .await
        .context("ensure veda_tunnel_bots")?;
        // Columns added after the first tunnel release; MySQL 8 lacks
        // ADD COLUMN IF NOT EXISTS, so consult information_schema.
        let have: Vec<String> = sqlx::query(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'veda_tunnel_bots'",
        )
        .fetch_all(&self.pool)
        .await
        .context("read veda_tunnel_bots columns")?
        .iter()
        .map(|r| r.try_get::<String, _>("COLUMN_NAME").unwrap_or_default())
        .collect();
        for (col, ddl) in [
            ("key_id", "ADD COLUMN key_id VARCHAR(64) NULL"),
            ("creator", "ADD COLUMN creator VARCHAR(128) NULL"),
            ("creator_name", "ADD COLUMN creator_name VARCHAR(128) NULL"),
            (
                "conn_state",
                "ADD COLUMN conn_state VARCHAR(16) NOT NULL DEFAULT 'unknown'",
            ),
            (
                "conn_updated_at",
                "ADD COLUMN conn_updated_at TIMESTAMP NULL",
            ),
        ] {
            if !have.iter().any(|c| c == col) {
                if let Err(e) = sqlx::query(&format!("ALTER TABLE veda_tunnel_bots {ddl}"))
                    .execute(&self.pool)
                    .await
                {
                    // tunnel runs the same migration; a concurrent start can
                    // lose the ALTER race with 1060 duplicate column — that's
                    // convergence, not failure.
                    let dup = e
                        .as_database_error()
                        .map(|d| d.message().contains("Duplicate column"))
                        .unwrap_or(false);
                    if !dup {
                        return Err(e)
                            .with_context(|| format!("migrate veda_tunnel_bots: add {col}"));
                    }
                }
            }
        }
        Ok(())
    }

    const COLS: &'static str = "bot_id, name, veda_key, workspace, project, mode, search_limit, \
         key_id, creator, creator_name, conn_state, conn_updated_at, created_at, updated_at";

    pub async fn list_by_project(&self, project_id: &str) -> Result<Vec<TunnelBotRow>, VedaError> {
        let rows = sqlx::query(&format!(
            "SELECT {} FROM veda_tunnel_bots WHERE project = ? ORDER BY name",
            Self::COLS
        ))
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.iter().map(row_to_view).collect()
    }

    /// Fetch one bot scoped to a project — a bot_id under another project
    /// reads as absent, so cross-tenant probing collapses to NOT_FOUND.
    pub async fn get_in_project(
        &self,
        bot_id: &str,
        project_id: &str,
    ) -> Result<Option<TunnelBotRow>, VedaError> {
        let row = sqlx::query(&format!(
            "SELECT {} FROM veda_tunnel_bots WHERE bot_id = ? AND project = ?",
            Self::COLS
        ))
        .bind(bot_id)
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        row.as_ref().map(row_to_view).transpose()
    }

    /// Insert; duplicate bot_id or name → AlreadyExists (MySQL 1062).
    pub async fn insert(&self, b: &NewTunnelBot) -> Result<(), VedaError> {
        let res = sqlx::query(
            "INSERT INTO veda_tunnel_bots \
             (bot_id, name, secret, veda_key, workspace, project, mode, search_limit, \
              key_id, creator, creator_name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.bot_id)
        .bind(&b.name)
        .bind(&b.secret)
        .bind(&b.veda_key)
        .bind(&b.workspace)
        .bind(&b.project)
        .bind(&b.mode)
        .bind(b.search_limit)
        .bind(&b.key_id)
        .bind(&b.creator)
        .bind(&b.creator_name)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate(&e) => Err(VedaError::AlreadyExists(format!(
                "bot '{}' (bot_id or name already configured)",
                b.bot_id
            ))),
            Err(e) => Err(internal(e)),
        }
    }

    /// PATCH scoped to a project. Returns false when the (bot_id, project)
    /// pair matches nothing.
    pub async fn update_in_project(
        &self,
        bot_id: &str,
        project_id: &str,
        p: &TunnelBotPatch,
    ) -> Result<bool, VedaError> {
        let res = sqlx::query(
            "UPDATE veda_tunnel_bots SET \
             name         = COALESCE(?, name), \
             secret       = COALESCE(NULLIF(?, ''), secret), \
             mode         = COALESCE(?, mode), \
             search_limit = COALESCE(?, search_limit) \
             WHERE bot_id = ? AND project = ?",
        )
        .bind(&p.name)
        .bind(&p.secret)
        .bind(&p.mode)
        .bind(p.search_limit)
        .bind(bot_id)
        .bind(project_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(r) => Ok(r.rows_affected() > 0),
            Err(e) if is_duplicate(&e) => {
                Err(VedaError::AlreadyExists("bot name already in use".into()))
            }
            Err(e) => Err(internal(e)),
        }
    }

    /// Delete scoped to a project; returns the row's `key_id` (for the caller
    /// to revoke the auto-minted wk_), or None when nothing matched. The
    /// affected-rows check closes the read→delete window: if another writer
    /// swapped the row in between, we must not report success (and must not
    /// revoke a key for a row we didn't actually delete).
    pub async fn delete_in_project(
        &self,
        bot_id: &str,
        project_id: &str,
    ) -> Result<Option<Option<String>>, VedaError> {
        let Some(existing) = self.get_in_project(bot_id, project_id).await? else {
            return Ok(None);
        };
        let res = sqlx::query("DELETE FROM veda_tunnel_bots WHERE bot_id = ? AND project = ?")
            .bind(bot_id)
            .bind(project_id)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        if res.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(existing.key_id))
    }

    /// Cascade for project deletion: drop every bot row bound to the project
    /// so the tunnel stops their WeCom connections on its next poll. The
    /// bots' minted keys need no separate revoke — project deletion revokes
    /// all workspace keys already. Returns the number of bots removed.
    pub async fn delete_by_project(&self, project_id: &str) -> Result<u64, VedaError> {
        let res = sqlx::query("DELETE FROM veda_tunnel_bots WHERE project = ?")
            .bind(project_id)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(res.rows_affected())
    }
}

fn is_duplicate(e: &sqlx::Error) -> bool {
    matches!(e.as_database_error().and_then(|d| d.code()), Some(c) if c == "23000")
        || e.as_database_error()
            .map(|d| d.message().contains("Duplicate entry"))
            .unwrap_or(false)
}

fn internal(e: impl std::fmt::Display) -> VedaError {
    VedaError::Internal(format!("tunnel bots store: {e}"))
}

fn row_to_view(row: &MySqlRow) -> Result<TunnelBotRow, VedaError> {
    let take = || -> Result<TunnelBotRow, sqlx::Error> {
        Ok(TunnelBotRow {
            bot_id: row.try_get("bot_id")?,
            name: row.try_get("name")?,
            veda_key: row.try_get("veda_key")?,
            workspace: row.try_get("workspace")?,
            project: row.try_get("project")?,
            mode: row.try_get("mode")?,
            search_limit: row.try_get("search_limit")?,
            key_id: row.try_get("key_id")?,
            creator: row.try_get("creator")?,
            creator_name: row.try_get("creator_name")?,
            conn_state: row.try_get("conn_state")?,
            conn_updated_at: row.try_get("conn_updated_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    };
    take().map_err(internal)
}

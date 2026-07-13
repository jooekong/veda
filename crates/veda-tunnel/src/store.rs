//! Bot config persistence in MySQL (`veda_tunnel_bots`).
//!
//! tunnel connects to the same MySQL instance as veda (its own table), using
//! sqlx — the same client + rustls stack as veda-store. This table is the
//! source of truth for the bot fleet: the admin CRUD API mutates it and the
//! control loop reflects changes onto live connections. `secret`/`veda_key`
//! are stored in clear (like the old tunnel.toml) — the table lives in
//! veda's MySQL, protected the same way as its credential tables.

use anyhow::{Context, Result};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::Row;

use crate::config::BotConfig;

pub struct BotStore {
    pool: MySqlPool,
}

impl BotStore {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .context("connect tunnel MySQL")?;
        let store = Self { pool };
        store.bootstrap().await?;
        Ok(store)
    }

    /// Create/upgrade the table. tunnel OWNS this DDL; veda-server's
    /// `tunnel_bots.rs` (platform write path) duplicates it so either side can
    /// start first — keep the two in sync when columns change.
    ///
    /// Columns beyond the original 8 config fields:
    /// - `key_id` / `creator` / `creator_name`: stamped by the platform API
    ///   when a bot is created through the AI Workbench (key_id lets DELETE
    ///   revoke the auto-minted `wk_`). NULL for bots added via the tunnel
    ///   admin console.
    /// - `conn_state` / `conn_updated_at`: tunnel's connection heartbeat,
    ///   written by the store-poll loop so the platform API can show liveness
    ///   without calling into tunnel.
    async fn bootstrap(&self) -> Result<()> {
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
        .context("bootstrap veda_tunnel_bots")?;
        self.migrate().await
    }

    /// Add columns introduced after the first release to a pre-existing table.
    /// MySQL 8 has no `ADD COLUMN IF NOT EXISTS`, so consult
    /// information_schema. Idempotent — safe on every start.
    async fn migrate(&self) -> Result<()> {
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
                    // veda-server runs the same migration; if both processes
                    // start against a pre-upgrade table at once, the loser's
                    // ALTER hits 1060 duplicate column — that's convergence,
                    // not failure.
                    if !is_duplicate_column(&e) {
                        return Err(e)
                            .with_context(|| format!("migrate veda_tunnel_bots: add {col}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Heartbeat: record the live connection state for the platform API to
    /// read. `updated_at = updated_at` pins the config-change timestamp —
    /// without it the ON UPDATE clause would refresh it every heartbeat.
    pub async fn set_conn_state(&self, bot_id: &str, state: &str) -> Result<()> {
        sqlx::query(
            "UPDATE veda_tunnel_bots \
             SET conn_state = ?, conn_updated_at = NOW(), updated_at = updated_at \
             WHERE bot_id = ?",
        )
        .bind(state)
        .bind(bot_id)
        .execute(&self.pool)
        .await
        .context("set conn_state")?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<BotConfig>> {
        let rows = sqlx::query(
            "SELECT bot_id, name, secret, veda_key, workspace, project, mode, search_limit \
             FROM veda_tunnel_bots ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .context("list bots")?;
        rows.iter().map(row_to_bot).collect()
    }

    pub async fn get(&self, bot_id: &str) -> Result<Option<BotConfig>> {
        let row = sqlx::query(
            "SELECT bot_id, name, secret, veda_key, workspace, project, mode, search_limit \
             FROM veda_tunnel_bots WHERE bot_id = ?",
        )
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await
        .context("get bot")?;
        row.as_ref().map(row_to_bot).transpose()
    }

    pub async fn count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM veda_tunnel_bots")
            .fetch_one(&self.pool)
            .await
            .context("count bots")?;
        Ok(row.try_get::<i64, _>("n")?)
    }

    /// Insert a new bot. Errors on duplicate bot_id / name.
    pub async fn add(&self, b: &BotConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO veda_tunnel_bots \
             (bot_id, name, secret, veda_key, workspace, project, mode, search_limit) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.bot_id)
        .bind(&b.name)
        .bind(&b.secret)
        .bind(&b.veda_key)
        .bind(&b.workspace)
        .bind(&b.project)
        .bind(&b.mode)
        .bind(b.limit as i32)
        .execute(&self.pool)
        .await
        .context("insert bot")?;
        Ok(())
    }

    /// Update an existing bot. Empty `secret` / `veda_key` keep the stored
    /// value (via `COALESCE(NULLIF(?, ''), col)`) so the UI never has to
    /// round-trip a plaintext secret or the full key. Returns false if
    /// bot_id is unknown.
    pub async fn update(&self, b: &BotConfig) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE veda_tunnel_bots SET \
             name=?, \
             secret   = COALESCE(NULLIF(?, ''), secret), \
             veda_key = COALESCE(NULLIF(?, ''), veda_key), \
             workspace=?, project=?, mode=?, search_limit=? \
             WHERE bot_id=?",
        )
        .bind(&b.name)
        .bind(&b.secret)
        .bind(&b.veda_key)
        .bind(&b.workspace)
        .bind(&b.project)
        .bind(&b.mode)
        .bind(b.limit as i32)
        .bind(&b.bot_id)
        .execute(&self.pool)
        .await
        .context("update bot")?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn remove(&self, bot_id: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM veda_tunnel_bots WHERE bot_id = ?")
            .bind(bot_id)
            .execute(&self.pool)
            .await
            .context("delete bot")?;
        Ok(res.rows_affected() > 0)
    }
}

/// MySQL 1060 "Duplicate column name" — the concurrent-migration race.
fn is_duplicate_column(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.message().contains("Duplicate column"))
        .unwrap_or(false)
}

fn row_to_bot(row: &MySqlRow) -> Result<BotConfig> {
    Ok(BotConfig {
        bot_id: row.try_get("bot_id")?,
        name: row.try_get("name")?,
        secret: row.try_get("secret")?,
        veda_key: row.try_get("veda_key")?,
        workspace: row.try_get("workspace")?,
        project: row.try_get("project")?,
        mode: row.try_get("mode")?,
        limit: row.try_get::<i32, _>("search_limit")? as usize,
    })
}

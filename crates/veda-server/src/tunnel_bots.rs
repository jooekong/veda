//! Platform-side access to `veda_tunnel_bots` — the WeCom bot table SHARED
//! with veda-tunnel (see `crates/veda-tunnel/src/store.rs`, which owns the
//! DDL; the CREATE here is a byte-for-byte copy so whichever process starts
//! first bootstraps it — keep them in sync).
//!
//! Also serves the platform's read-only view of the tunnel's QA telemetry
//! (`veda_tunnel_qa_log` / `veda_tunnel_qa_feedback`, owner:
//! `veda-tunnel/src/qa_log.rs`) so the AI Workbench can show per-project stats
//! and Q&A detail. Those tables carry only `bot_id`, so tenant isolation is
//! enforced by the caller resolving a project's bots first and constraining
//! every read to that `bot_id` set — see `qa_stats` / `qa_logs`.
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
use serde::Serialize;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::Row;
use veda_types::VedaError;

/// QA feedback kinds as stored by veda-tunnel (`qa_log.rs`).
const QA_FEEDBACK_UP: i8 = 1;
const QA_FEEDBACK_DOWN: i8 = 2;

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
    /// Custom answer persona; NULL → server default persona.
    pub prompt: Option<String>,
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
    pub prompt: Option<String>,
    pub key_id: String,
    pub creator: Option<String>,
    pub creator_name: Option<String>,
}

/// Patchable fields (PATCH semantics: `None` = keep). For `prompt`,
/// `Some("")` clears the custom persona back to the server default.
#[derive(Default)]
pub struct TunnelBotPatch {
    pub name: Option<String>,
    pub secret: Option<String>,
    pub mode: Option<String>,
    pub search_limit: Option<i32>,
    pub prompt: Option<String>,
}

/// QA telemetry summary over a window (mirrors veda-tunnel's `QaStats`).
#[derive(Debug, Serialize)]
pub struct QaStats {
    pub days: u32,
    pub total: i64,
    /// outcome → count, e.g. `{"answered": 120, "no_context": 8}`.
    pub outcomes: serde_json::Map<String, serde_json::Value>,
    pub feedback_up: i64,
    pub feedback_down: i64,
}

/// One Q&A row with its per-row vote counts (mirrors veda-tunnel's `QaLogRow`).
/// `answer_text` is returned verbatim (MEDIUMTEXT; page size caps the blast).
#[derive(Debug, Serialize)]
pub struct QaLogRow {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub bot_id: String,
    pub chat_type: String,
    pub user_id: String,
    pub query: String,
    pub outcome: String,
    pub hit_count: i32,
    pub citation_count: i32,
    pub latency_ms: i32,
    pub answer_text: Option<String>,
    /// JSON array of the tool calls behind the answer (search queries /
    /// file reads, in order); null for pre-trace rows and non-streamed
    /// replies.
    pub tool_trace: Option<String>,
    pub up_count: i64,
    pub down_count: i64,
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
        store.ensure_qa_schema().await?;
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
                prompt       TEXT         NULL,
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
            ("prompt", "ADD COLUMN prompt TEXT NULL"),
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

    /// Idempotent bootstrap of the QA telemetry tables — a byte-for-byte copy
    /// of veda-tunnel's `qa_log.rs` DDL + column migration (owner:
    /// veda-tunnel; column changes must land in both). A veda-server carrying
    /// this read API can land on a fresh DB before the tunnel that writes
    /// these rows.
    async fn ensure_qa_schema(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS veda_tunnel_qa_log (
                id             BIGINT AUTO_INCREMENT PRIMARY KEY,
                ts             TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
                bot_id         VARCHAR(128) NOT NULL,
                chat_type      VARCHAR(16)  NOT NULL,
                chat_key       VARCHAR(191) NOT NULL,
                user_id        VARCHAR(128) NOT NULL,
                query          TEXT         NOT NULL,
                outcome        VARCHAR(16)  NOT NULL,
                hit_count      INT          NOT NULL DEFAULT 0,
                citation_count INT          NOT NULL DEFAULT 0,
                latency_ms     INT          NOT NULL DEFAULT 0,
                answer_text    MEDIUMTEXT   NULL,
                feedback_id    VARCHAR(64)  NULL,
                tool_trace     TEXT         NULL,
                KEY idx_bot_ts (bot_id, ts),
                KEY idx_outcome (outcome, ts),
                KEY idx_feedback (feedback_id)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
            "#,
        )
        .execute(&self.pool)
        .await
        .context("ensure veda_tunnel_qa_log")?;
        // Columns added after the table first shipped; veda-tunnel runs the
        // same migration — losing the ALTER race with 1060 duplicate column
        // is convergence, not failure.
        let have: Vec<String> = sqlx::query(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'veda_tunnel_qa_log'",
        )
        .fetch_all(&self.pool)
        .await
        .context("read veda_tunnel_qa_log columns")?
        .iter()
        .map(|r| r.try_get::<String, _>("COLUMN_NAME").unwrap_or_default())
        .collect();
        for (col, ddl) in [("tool_trace", "ADD COLUMN tool_trace TEXT NULL")] {
            if !have.iter().any(|c| c == col) {
                if let Err(e) = sqlx::query(&format!("ALTER TABLE veda_tunnel_qa_log {ddl}"))
                    .execute(&self.pool)
                    .await
                {
                    let dup = e
                        .as_database_error()
                        .map(|d| d.message().contains("Duplicate column"))
                        .unwrap_or(false);
                    if !dup {
                        return Err(e)
                            .with_context(|| format!("migrate veda_tunnel_qa_log: add {col}"));
                    }
                }
            }
        }
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS veda_tunnel_qa_feedback (
                id          BIGINT AUTO_INCREMENT PRIMARY KEY,
                ts          TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
                feedback_id VARCHAR(64)  NOT NULL,
                user_id     VARCHAR(128) NOT NULL,
                kind        TINYINT      NOT NULL,
                reason      TINYINT      NULL,
                KEY idx_feedback (feedback_id),
                UNIQUE KEY uk_fb_user (feedback_id, user_id)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
            "#,
        )
        .execute(&self.pool)
        .await
        .context("ensure veda_tunnel_qa_feedback")?;
        Ok(())
    }

    const COLS: &'static str = "bot_id, name, veda_key, workspace, project, mode, search_limit, \
         prompt, key_id, creator, creator_name, conn_state, conn_updated_at, created_at, updated_at";

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
              prompt, key_id, creator, creator_name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&b.bot_id)
        .bind(&b.name)
        .bind(&b.secret)
        .bind(&b.veda_key)
        .bind(&b.workspace)
        .bind(&b.project)
        .bind(&b.mode)
        .bind(b.search_limit)
        .bind(&b.prompt)
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
        // prompt: absent = keep, "" = clear back to the server default
        // persona, non-empty = set. The CASE keeps "absent" distinct from
        // "clear", which COALESCE alone cannot express.
        let res = sqlx::query(
            "UPDATE veda_tunnel_bots SET \
             name         = COALESCE(?, name), \
             secret       = COALESCE(NULLIF(?, ''), secret), \
             mode         = COALESCE(?, mode), \
             search_limit = COALESCE(?, search_limit), \
             prompt       = CASE WHEN ? IS NULL THEN prompt ELSE NULLIF(?, '') END \
             WHERE bot_id = ? AND project = ?",
        )
        .bind(&p.name)
        .bind(&p.secret)
        .bind(&p.mode)
        .bind(p.search_limit)
        .bind(&p.prompt)
        .bind(&p.prompt)
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

    // ── QA telemetry reads (tenant scope = a project's bots) ────────────────

    /// The `bot_id`s under a project — the QA read scope. A project with no
    /// bots yields an empty vec, which the QA queries short-circuit to empty
    /// results (never an error).
    pub async fn bot_ids_by_project(&self, project_id: &str) -> Result<Vec<String>, VedaError> {
        let rows = sqlx::query("SELECT bot_id FROM veda_tunnel_bots WHERE project = ?")
            .bind(project_id)
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("bot_id").map_err(internal))
            .collect()
    }

    /// Outcome distribution + thumb up/down over the last `days`, constrained to
    /// `bot_ids` (the project's bots). Empty scope → zeroed stats, no query.
    pub async fn qa_stats(&self, bot_ids: &[String], days: u32) -> Result<QaStats, VedaError> {
        let mut outcomes = serde_json::Map::new();
        if bot_ids.is_empty() {
            return Ok(QaStats {
                days,
                total: 0,
                outcomes,
                feedback_up: 0,
                feedback_down: 0,
            });
        }
        let ph = in_placeholders(bot_ids.len());
        let outcome_sql = format!(
            "SELECT outcome, COUNT(*) AS n FROM veda_tunnel_qa_log \
             WHERE ts >= NOW() - INTERVAL ? DAY AND bot_id IN ({ph}) \
             GROUP BY outcome"
        );
        let mut q = sqlx::query(&outcome_sql).bind(days);
        for b in bot_ids {
            q = q.bind(b);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(internal)?;
        let mut total = 0i64;
        for r in &rows {
            let outcome: String = r.try_get("outcome").map_err(internal)?;
            let n: i64 = r.try_get("n").map_err(internal)?;
            total += n;
            outcomes.insert(outcome, n.into());
        }
        let fb_sql = format!(
            "SELECT f.kind, COUNT(*) AS n FROM veda_tunnel_qa_feedback f \
             JOIN veda_tunnel_qa_log q ON q.feedback_id = f.feedback_id \
             WHERE q.ts >= NOW() - INTERVAL ? DAY AND q.bot_id IN ({ph}) \
             GROUP BY f.kind"
        );
        let mut q = sqlx::query(&fb_sql).bind(days);
        for b in bot_ids {
            q = q.bind(b);
        }
        let fb = q.fetch_all(&self.pool).await.map_err(internal)?;
        let (mut up, mut down) = (0i64, 0i64);
        for r in &fb {
            let kind: i8 = r.try_get("kind").map_err(internal)?;
            let n: i64 = r.try_get("n").map_err(internal)?;
            if kind == QA_FEEDBACK_UP {
                up = n;
            } else if kind == QA_FEEDBACK_DOWN {
                down = n;
            }
        }
        Ok(QaStats {
            days,
            total,
            outcomes,
            feedback_up: up,
            feedback_down: down,
        })
    }

    /// Newest-first Q&A rows (with per-row vote counts) plus the total matching
    /// the filter, constrained to `bot_ids`. Empty scope → empty page, no query.
    /// `page` is 1-based; `size` is clamped defensively to 1..=100.
    pub async fn qa_logs(
        &self,
        bot_ids: &[String],
        outcome: Option<&str>,
        down_voted: bool,
        page: u32,
        size: u32,
    ) -> Result<(Vec<QaLogRow>, i64), VedaError> {
        if bot_ids.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let size = size.clamp(1, 100);
        // u64 arithmetic: a caller-supplied huge `page` must not overflow u32
        // (release builds would wrap into a bogus offset).
        let offset = (u64::from(page.max(1)) - 1) * u64::from(size);
        let ph = in_placeholders(bot_ids.len());
        let dv = down_voted as i32;

        // Total for the company page envelope (exact, over the same filter).
        let count_sql = format!(
            "SELECT COUNT(*) AS n FROM veda_tunnel_qa_log q \
             WHERE q.bot_id IN ({ph}) \
               AND (? IS NULL OR q.outcome = ?) \
               AND (? = 0 OR EXISTS (SELECT 1 FROM veda_tunnel_qa_feedback f \
                    WHERE f.feedback_id = q.feedback_id AND f.kind = 2))"
        );
        let mut q = sqlx::query(&count_sql);
        for b in bot_ids {
            q = q.bind(b);
        }
        let total: i64 = q
            .bind(outcome)
            .bind(outcome)
            .bind(dv)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?
            .try_get("n")
            .map_err(internal)?;

        let list_sql = format!(
            "SELECT q.id, q.ts, q.bot_id, q.chat_type, q.user_id, q.query, q.outcome, \
                    q.hit_count, q.citation_count, q.latency_ms, q.answer_text, q.tool_trace, \
                    (SELECT COUNT(*) FROM veda_tunnel_qa_feedback f \
                     WHERE f.feedback_id = q.feedback_id AND f.kind = 1) AS up_count, \
                    (SELECT COUNT(*) FROM veda_tunnel_qa_feedback f \
                     WHERE f.feedback_id = q.feedback_id AND f.kind = 2) AS down_count \
             FROM veda_tunnel_qa_log q \
             WHERE q.bot_id IN ({ph}) \
               AND (? IS NULL OR q.outcome = ?) \
               AND (? = 0 OR EXISTS (SELECT 1 FROM veda_tunnel_qa_feedback f \
                    WHERE f.feedback_id = q.feedback_id AND f.kind = 2)) \
             ORDER BY q.id DESC LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query(&list_sql);
        for b in bot_ids {
            q = q.bind(b);
        }
        let rows = q
            .bind(outcome)
            .bind(outcome)
            .bind(dv)
            .bind(size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        let items = rows
            .iter()
            .map(qa_row_to_log)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((items, total))
    }
}

/// `?, ?, …` for an `IN (...)` clause of `n` bound values. Only the `?`
/// placeholders are interpolated — the values themselves are always bound, so
/// this is injection-safe. Callers guarantee `n >= 1`.
fn in_placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn qa_row_to_log(r: &MySqlRow) -> Result<QaLogRow, VedaError> {
    let take = || -> Result<QaLogRow, sqlx::Error> {
        Ok(QaLogRow {
            id: r.try_get("id")?,
            ts: r.try_get("ts")?,
            bot_id: r.try_get("bot_id")?,
            chat_type: r.try_get("chat_type")?,
            user_id: r.try_get("user_id")?,
            query: r.try_get("query")?,
            outcome: r.try_get("outcome")?,
            hit_count: r.try_get("hit_count")?,
            citation_count: r.try_get("citation_count")?,
            latency_ms: r.try_get("latency_ms")?,
            answer_text: r.try_get("answer_text")?,
            tool_trace: r.try_get("tool_trace")?,
            up_count: r.try_get("up_count")?,
            down_count: r.try_get("down_count")?,
        })
    };
    take().map_err(internal)
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
            prompt: row.try_get("prompt")?,
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

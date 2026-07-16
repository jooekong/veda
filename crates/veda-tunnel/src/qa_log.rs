//! QA telemetry (plan: docs/plans/veda-tunnel-qa-log.md).
//!
//! Every answered question lands one row in `veda_tunnel_qa_log` (query,
//! answer text, outcome, latency); WeCom thumb-up/down callbacks land in
//! `veda_tunnel_qa_feedback` keyed by the `feedback.id` uuid we attach to
//! each reply's first stream frame. `no_context` rows double as the
//! "missing docs" backlog; down-voted + error rows are the bad-case list.
//!
//! Writes are best-effort — a logging failure must never break the reply
//! path (callers warn and move on). Reads back the admin surface
//! (`/admin/stats`, `/admin/qa-log`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::Row;

pub struct QaLogStore {
    pool: MySqlPool,
}

/// One reply, recorded after the final stream frame is sent.
pub struct QaLogEntry {
    pub bot_id: String,
    /// `single` | `group`.
    pub chat_type: String,
    /// group → chatid, single → userid. Doubles as the reachable-chat key
    /// for future proactive push (tunnel-directions T6).
    pub chat_key: String,
    pub user_id: String,
    pub query: String,
    pub outcome: &'static str,
    pub hit_count: u32,
    pub citation_count: u32,
    pub latency_ms: u32,
    /// The exact text sent to WeCom — including canned error/no-context
    /// phrases, so the log always shows "what the user saw".
    pub answer_text: String,
    pub feedback_id: String,
    /// JSON array of the tool calls the server announced while answering
    /// (`[{"tool":"search","detail":"…"}]`, in execution order) — how the
    /// answer was assembled. `None` when no tool ran or the reply didn't
    /// stream (one-shot fallback / errors).
    pub tool_trace: Option<String>,
}

/// Feedback kinds as stored. Wire values from the WeCom `feedback_event`
/// are mapped by the caller (conn.rs) once the real payload is confirmed
/// on a live bot — see plan §8 step 3.
pub const FEEDBACK_UP: i8 = 1;
pub const FEEDBACK_DOWN: i8 = 2;

#[derive(Debug, Serialize)]
pub struct QaStats {
    pub days: u32,
    pub total: i64,
    /// outcome → count, e.g. {"answered": 120, "no_context": 8}.
    pub outcomes: serde_json::Map<String, serde_json::Value>,
    pub feedback_up: i64,
    pub feedback_down: i64,
}

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
    /// See [`QaLogEntry::tool_trace`] — raw JSON array or null.
    pub tool_trace: Option<String>,
    pub up_count: i64,
    pub down_count: i64,
}

#[derive(Default)]
pub struct QaLogFilter {
    /// Exact outcome match, e.g. `no_context`.
    pub outcome: Option<String>,
    /// Only rows with at least one down-vote.
    pub down_voted: bool,
    pub bot_id: Option<String>,
    pub page: u32,
    pub size: u32,
}

impl QaLogStore {
    /// Share the bot store's pool — same MySQL, no extra connections.
    pub async fn new(pool: MySqlPool) -> Result<Self> {
        let store = Self { pool };
        store.bootstrap().await?;
        Ok(store)
    }

    /// Idempotent CREATEs plus the column migration for pre-existing tables
    /// (same information_schema dance as store.rs — MySQL 8 has no
    /// `ADD COLUMN IF NOT EXISTS`). veda-server's `tunnel_bots.rs` carries a
    /// copy of this DDL + migration; column changes must land in both.
    async fn bootstrap(&self) -> Result<()> {
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
        .context("bootstrap veda_tunnel_qa_log")?;
        self.migrate_qa_log_columns().await?;
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
        .context("bootstrap veda_tunnel_qa_feedback")?;
        Ok(())
    }

    /// Columns added after the tables first shipped. Both this process and
    /// veda-server run the same migration — losing the ALTER race with 1060
    /// duplicate column is convergence, not failure.
    async fn migrate_qa_log_columns(&self) -> Result<()> {
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
        Ok(())
    }

    pub async fn log(&self, e: &QaLogEntry) -> Result<()> {
        sqlx::query(
            "INSERT INTO veda_tunnel_qa_log \
             (bot_id, chat_type, chat_key, user_id, query, outcome, hit_count, \
              citation_count, latency_ms, answer_text, feedback_id, tool_trace) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.bot_id)
        .bind(&e.chat_type)
        .bind(&e.chat_key)
        .bind(&e.user_id)
        .bind(&e.query)
        .bind(e.outcome)
        .bind(e.hit_count)
        .bind(e.citation_count)
        .bind(e.latency_ms)
        .bind(&e.answer_text)
        .bind(&e.feedback_id)
        .bind(&e.tool_trace)
        .execute(&self.pool)
        .await
        .context("insert qa log")?;
        Ok(())
    }

    /// Same user re-voting replaces their previous vote (uk_fb_user).
    /// Stored even when no qa_log row matches — never drop feedback.
    pub async fn upsert_feedback(
        &self,
        feedback_id: &str,
        user_id: &str,
        kind: i8,
        reason: Option<i8>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO veda_tunnel_qa_feedback (feedback_id, user_id, kind, reason) \
             VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE kind = VALUES(kind), reason = VALUES(reason), ts = NOW()",
        )
        .bind(feedback_id)
        .bind(user_id)
        .bind(kind)
        .bind(reason)
        .execute(&self.pool)
        .await
        .context("upsert qa feedback")?;
        Ok(())
    }

    pub async fn stats(&self, days: u32, bot_id: Option<&str>) -> Result<QaStats> {
        let mut outcomes = serde_json::Map::new();
        let mut total: i64 = 0;
        let rows = sqlx::query(
            "SELECT outcome, COUNT(*) AS n FROM veda_tunnel_qa_log \
             WHERE ts >= NOW() - INTERVAL ? DAY AND (? IS NULL OR bot_id = ?) \
             GROUP BY outcome",
        )
        .bind(days)
        .bind(bot_id)
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await
        .context("stats outcomes")?;
        for r in &rows {
            let outcome: String = r.try_get("outcome")?;
            let n: i64 = r.try_get("n")?;
            total += n;
            outcomes.insert(outcome, n.into());
        }
        let fb = sqlx::query(
            "SELECT f.kind, COUNT(*) AS n FROM veda_tunnel_qa_feedback f \
             JOIN veda_tunnel_qa_log q ON q.feedback_id = f.feedback_id \
             WHERE q.ts >= NOW() - INTERVAL ? DAY AND (? IS NULL OR q.bot_id = ?) \
             GROUP BY f.kind",
        )
        .bind(days)
        .bind(bot_id)
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await
        .context("stats feedback")?;
        let (mut up, mut down) = (0i64, 0i64);
        for r in &fb {
            let kind: i8 = r.try_get("kind")?;
            let n: i64 = r.try_get("n")?;
            if kind == FEEDBACK_UP {
                up = n;
            } else if kind == FEEDBACK_DOWN {
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

    pub async fn list(&self, f: &QaLogFilter) -> Result<Vec<QaLogRow>> {
        let size = f.size.clamp(1, 100);
        // u64 arithmetic: a huge `page` must not overflow u32 (release builds
        // would wrap into a bogus offset).
        let offset = (u64::from(f.page.max(1)) - 1) * u64::from(size);
        let rows = sqlx::query(
            "SELECT q.id, q.ts, q.bot_id, q.chat_type, q.user_id, q.query, q.outcome, \
                    q.hit_count, q.citation_count, q.latency_ms, q.answer_text, q.tool_trace, \
                    (SELECT COUNT(*) FROM veda_tunnel_qa_feedback f \
                     WHERE f.feedback_id = q.feedback_id AND f.kind = 1) AS up_count, \
                    (SELECT COUNT(*) FROM veda_tunnel_qa_feedback f \
                     WHERE f.feedback_id = q.feedback_id AND f.kind = 2) AS down_count \
             FROM veda_tunnel_qa_log q \
             WHERE (? IS NULL OR q.outcome = ?) \
               AND (? IS NULL OR q.bot_id = ?) \
               AND (? = 0 OR EXISTS (SELECT 1 FROM veda_tunnel_qa_feedback f \
                    WHERE f.feedback_id = q.feedback_id AND f.kind = 2)) \
             ORDER BY q.id DESC LIMIT ? OFFSET ?",
        )
        .bind(&f.outcome)
        .bind(&f.outcome)
        .bind(&f.bot_id)
        .bind(&f.bot_id)
        .bind(f.down_voted as i32)
        .bind(size)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .context("list qa log")?;
        rows.iter().map(row_to_log).collect()
    }
}

fn row_to_log(r: &MySqlRow) -> Result<QaLogRow> {
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
}

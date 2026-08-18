//! MySQL-backed metadata store, transactional metadata, and outbox task queue.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::types::Json;
use sqlx::{MySqlPool, Row, Transaction};
use veda_core::store::{
    AuthStore, CollectionMetaStore, DentryPathRef, DocAccessOrder, DocAccessRow, MetadataStore,
    MetadataTx, TaskQueue,
};
use veda_types::{
    Account, ApiKeyRecord, CollectionSchema, Dataset, Dentry, FileChunk, FileExtract, FileRecord,
    FileSummary, FsEvent, OutboxEvent, OutboxEventType, OutboxStatus, Result, SourceType,
    StorageStats, StorageType,
    SummaryStatus, VedaError, Workspace, WorkspaceKey,
};

// Batched INSERT sizes. FS events are tiny rows; 500/batch keeps
// throughput high. File chunks are normally <= CHUNK_SIZE (256KB), so
// 50/batch is ~12.5MB — but split_and_hash's single-line fallback can
// emit one chunk up to the 50MB file cap (a >256KB span with no newline).
// A single file's chunks total <= MAX_FILE_BYTES (50MB), so one batch
// stays under MySQL 8's default max_allowed_packet (64MB); a deployment
// that lowers it below ~50MB can fail the INSERT for such a file.
const CHUNK_INSERT_BATCH: usize = 50;
const FS_EVENT_INSERT_BATCH: usize = 500;

fn storage_err(e: impl std::fmt::Display) -> VedaError {
    let msg = e.to_string();
    if msg.contains("1213") || msg.contains("Deadlock") {
        return VedaError::Deadlock(msg);
    }
    VedaError::Storage(msg)
}

/// True if the error is a MySQL UNIQUE/duplicate-key violation (errno 1062).
/// Uses `number()`, NOT `code()`: sqlx's `code()` returns the SQLSTATE
/// ("23000" for a dup key), never "1062", so a `code()=="1062"` check is dead.
fn is_mysql_duplicate(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db)
        if db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .map(|me| me.number() == 1062)
            .unwrap_or(false))
}

/// Variant of `storage_err` for UPDATEs on `veda_accounts.email` where
/// the unique index can fire on a race. Maps MySQL 1062 (ER_DUP_ENTRY)
/// to a typed `AlreadyExists`; everything else falls through to the
/// generic translator.
fn translate_account_email_conflict(e: sqlx::Error) -> VedaError {
    if is_mysql_duplicate(&e) {
        return VedaError::AlreadyExists("email already registered".into());
    }
    storage_err(e)
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Map a `snake_case` MySQL ENUM column back to its veda-types enum,
/// piggybacking on serde's `#[serde(rename_all = "snake_case")]` derive
/// so adding/removing an enum variant requires zero changes here.
fn db_enum<T: serde::de::DeserializeOwned>(field: &'static str, s: &str) -> Result<T> {
    serde_plain::from_str(s).map_err(|_| storage_err(format!("unknown {field}: {s}")))
}

/// Counterpart of `db_enum`. Unit-variant enums always serialize cleanly,
/// so the only failure mode is misuse — `expect` documents that contract.
fn db_enum_str<T: serde::Serialize>(val: &T) -> String {
    serde_plain::to_string(val).expect("unit enum must serialize to string")
}


fn order_clause(order_by: &str, order: &str) -> (&'static str, &'static str) {
    let col = match order_by {
        "id" => "id",
        _ => "created_at",
    };
    let dir = if order.eq_ignore_ascii_case("asc") {
        "ASC"
    } else {
        "DESC"
    };
    (col, dir)
}


pub struct MysqlStore {
    pool: MySqlPool,
}

/// Snapshot of pool counters for `veda_mysql_pool_*` gauges. Caller does the
/// `metrics::gauge!` set; we only return the numbers because `metrics` should
/// not be a hard dependency of this crate's public API (only at the crate root
/// for typed instrumentation).
pub struct PoolStats {
    pub size: u32,
    pub idle: usize,
}

impl MysqlStore {
    pub fn pool_stats(&self) -> PoolStats {
        PoolStats {
            size: self.pool.size(),
            idle: self.pool.num_idle(),
        }
    }

    /// Outbox row counts grouped by status, for the `veda_outbox_depth` gauge
    /// sampler. Restricted to the three actionable statuses so the periodic
    /// sample never counts the (potentially large) `completed` backlog before
    /// retention prunes it. `status` leads both outbox indexes (`idx_claim`,
    /// `idx_retention`), so this is an index range scan over those values.
    pub async fn outbox_status_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT status, COUNT(*) AS n FROM veda_outbox \
             WHERE status IN ('pending','processing','dead') GROUP BY status",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let status: String = r.try_get("status").map_err(storage_err)?;
            let n: i64 = r.try_get("n").map_err(storage_err)?;
            out.push((status, n));
        }
        Ok(out)
    }
}

/// Pool tuning parameters. Values of `0` for the optional knobs mean
/// "leave at sqlx default"; positive values are passed through. Sane
/// production defaults live in `veda-server::config::MysqlConfig`.
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    /// Time to wait for a free connection before failing. 0 = sqlx default (30s).
    pub acquire_timeout_secs: u64,
    /// Drop idle connections after this many seconds. 0 = no idle timeout.
    pub idle_timeout_secs: u64,
    /// Recycle every connection after this many seconds. 0 = no max lifetime.
    pub max_lifetime_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 50,
            min_connections: 0,
            acquire_timeout_secs: 0,
            idle_timeout_secs: 0,
            max_lifetime_secs: 0,
        }
    }
}


impl MysqlStore {
    pub async fn new(database_url: &str) -> Result<Self> {
        Self::with_pool_config(database_url, PoolConfig::default()).await
    }

    pub async fn with_pool_config(database_url: &str, cfg: PoolConfig) -> Result<Self> {
        let mut opts = sqlx::pool::PoolOptions::<sqlx::MySql>::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            // Pin every connection to UTC. The whole codebase binds
            // `chrono::DateTime<Utc>::naive_utc()` into NaiveDateTime columns;
            // sqlx writes those as the server's session timezone literal. If
            // the MySQL server runs in CST/PST, those values would silently
            // shift. Force +00:00 so the on-the-wire interpretation matches
            // the in-memory UTC contract.
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    conn.execute("SET time_zone = '+00:00'").await?;
                    Ok(())
                })
            });
        if cfg.acquire_timeout_secs > 0 {
            opts = opts.acquire_timeout(std::time::Duration::from_secs(cfg.acquire_timeout_secs));
        }
        if cfg.idle_timeout_secs > 0 {
            opts = opts.idle_timeout(std::time::Duration::from_secs(cfg.idle_timeout_secs));
        }
        if cfg.max_lifetime_secs > 0 {
            opts = opts.max_lifetime(std::time::Duration::from_secs(cfg.max_lifetime_secs));
        }
        let pool = opts
            .connect(database_url)
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }
}

mod auth;
mod collection;
mod conn;
mod memory;
mod metadata;
mod queue;
mod rows;
mod schema;
mod tx;

use conn::*;
use rows::*;

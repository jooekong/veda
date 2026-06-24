//! MySQL-backed metadata store, transactional metadata, and outbox task queue.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::types::Json;
use sqlx::{MySqlPool, Row, Transaction};
use veda_core::store::{AuthStore, CollectionMetaStore, MetadataStore, MetadataTx, TaskQueue};
use veda_types::{
    Account, ApiKeyRecord, CollectionSchema, Dataset, Dentry, FileChunk, FileRecord, FileSummary,
    FsEvent, OutboxEvent, OutboxEventType, OutboxStatus, Result, SourceType, StorageStats,
    StorageType,
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

fn row_to_fs_event(row: &sqlx::mysql::MySqlRow) -> Result<FsEvent> {
    let et: String = row.try_get("event_type").map_err(storage_err)?;
    Ok(FsEvent {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        event_type: db_enum("fs_event_type", &et)?,
        path: row.try_get("path").map_err(storage_err)?,
        file_id: row.try_get("file_id").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
    })
}

fn row_to_collection_schema(row: &sqlx::mysql::MySqlRow) -> Result<CollectionSchema> {
    let ct: String = row.try_get("collection_type").map_err(storage_err)?;
    let st: String = row.try_get("status").map_err(storage_err)?;
    let Json(schema_json): Json<serde_json::Value> =
        row.try_get("schema_json").map_err(storage_err)?;
    Ok(CollectionSchema {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        collection_type: db_enum("collection_type", &ct)?,
        schema_json,
        embedding_source: row.try_get("embedding_source").map_err(storage_err)?,
        embedding_dim: row.try_get("embedding_dim").map_err(storage_err)?,
        status: db_enum("collection_status", &st)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_account(row: &sqlx::mysql::MySqlRow) -> Result<Account> {
    let st: String = row.try_get("status").map_err(storage_err)?;
    Ok(Account {
        id: row.try_get("id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        email: row.try_get("email").map_err(storage_err)?,
        password_hash: row.try_get("password_hash").map_err(storage_err)?,
        app_id: row.try_get("app_id").map_err(storage_err)?,
        status: db_enum("account_status", &st)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

/// Whitelist the apps-surface `order_by` / `order` into safe SQL fragments —
/// caller input must never be interpolated into SQL raw. Defaults `created_at`
/// `DESC`.
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

fn row_to_dataset(row: &sqlx::mysql::MySqlRow) -> Result<Dataset> {
    let st: String = row.try_get("status").map_err(storage_err)?;
    Ok(Dataset {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        status: db_enum("dataset_status", &st)?,
        description: row.try_get("description").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_workspace(row: &sqlx::mysql::MySqlRow) -> Result<Workspace> {
    let st: String = row.try_get("status").map_err(storage_err)?;
    let kd: String = row.try_get("kind").map_err(storage_err)?;
    Ok(Workspace {
        id: row.try_get("id").map_err(storage_err)?,
        account_id: row.try_get("account_id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        status: db_enum("workspace_status", &st)?,
        kind: db_enum("workspace_kind", &kd)?,
        app_id: row.try_get("app_id").map_err(storage_err)?,
        description: row.try_get("description").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_api_key(row: &sqlx::mysql::MySqlRow) -> Result<ApiKeyRecord> {
    let st: String = row.try_get("status").map_err(storage_err)?;
    let allowed_raw: Option<Json<Vec<String>>> =
        row.try_get("allowed_workspaces").map_err(storage_err)?;
    Ok(ApiKeyRecord {
        id: row.try_get("id").map_err(storage_err)?,
        account_id: row.try_get("account_id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        key_hash: row.try_get("key_hash").map_err(storage_err)?,
        status: db_enum("key_status", &st)?,
        app_id: row.try_get("app_id").map_err(storage_err)?,
        allowed_workspaces: allowed_raw.map(|j| j.0),
        expires_at: row.try_get("expires_at").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
    })
}

fn row_to_workspace_key(row: &sqlx::mysql::MySqlRow) -> Result<WorkspaceKey> {
    let st: String = row.try_get("status").map_err(storage_err)?;
    let perm: String = row.try_get("permission").map_err(storage_err)?;
    let kd: String = row.try_get("kind").map_err(storage_err)?;
    Ok(WorkspaceKey {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        account_id: row.try_get("account_id").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        key_hash: row.try_get("key_hash").map_err(storage_err)?,
        permission: db_enum("key_permission", &perm)?,
        status: db_enum("key_status", &st)?,
        kind: db_enum("workspace_kind", &kd)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
    })
}

fn row_to_dentry(row: &sqlx::mysql::MySqlRow) -> Result<Dentry> {
    let file_id: Option<String> = row.try_get("file_id").map_err(storage_err)?;
    Ok(Dentry {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        parent_path: row.try_get("parent_path").map_err(storage_err)?,
        name: row.try_get("name").map_err(storage_err)?,
        path: row.try_get("path").map_err(storage_err)?,
        file_id,
        is_dir: row.try_get::<bool, _>("is_dir").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_file(row: &sqlx::mysql::MySqlRow) -> Result<FileRecord> {
    let st: String = row.try_get("storage_type").map_err(storage_err)?;
    let src: String = row.try_get("source_type").map_err(storage_err)?;
    Ok(FileRecord {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        size_bytes: row.try_get("size_bytes").map_err(storage_err)?,
        mime_type: row.try_get("mime_type").map_err(storage_err)?,
        storage_type: db_enum("storage_type", &st)?,
        source_type: db_enum("source_type", &src)?,
        line_count: row.try_get("line_count").map_err(storage_err)?,
        checksum_sha256: row.try_get("checksum_sha256").map_err(storage_err)?,
        revision: row.try_get("revision").map_err(storage_err)?,
        ref_count: row.try_get("ref_count").map_err(storage_err)?,
        last_embedded_content_hash: row
            .try_get("last_embedded_content_hash")
            .map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

fn row_to_file_chunk(row: &sqlx::mysql::MySqlRow) -> Result<FileChunk> {
    Ok(FileChunk {
        file_id: row.try_get("file_id").map_err(storage_err)?,
        chunk_index: row.try_get("chunk_index").map_err(storage_err)?,
        start_line: row.try_get("start_line").map_err(storage_err)?,
        line_count: row.try_get("line_count").map_err(storage_err)?,
        byte_len: row.try_get("byte_len").map_err(storage_err)?,
        chunk_sha256: row.try_get("chunk_sha256").map_err(storage_err)?,
        content: row.try_get("content").map_err(storage_err)?,
    })
}

fn row_to_outbox(row: &sqlx::mysql::MySqlRow) -> Result<OutboxEvent> {
    let et: String = row.try_get("event_type").map_err(storage_err)?;
    let st: String = row.try_get("status").map_err(storage_err)?;
    let Json(payload): Json<serde_json::Value> = row.try_get("payload").map_err(storage_err)?;
    Ok(OutboxEvent {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        event_type: db_enum("outbox_event_type", &et)?,
        payload,
        status: db_enum("outbox_status", &st)?,
        retry_count: row.try_get("retry_count").map_err(storage_err)?,
        max_retries: row.try_get("max_retries").map_err(storage_err)?,
        available_at: row.try_get("available_at").map_err(storage_err)?,
        lease_until: row.try_get("lease_until").map_err(storage_err)?,
        created_at: row.try_get("created_at").map_err(storage_err)?,
    })
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

    pub async fn migrate(&self) -> Result<()> {
        let stmts = [
            r#"CREATE TABLE IF NOT EXISTS veda_dentries (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    parent_path VARCHAR(4096) NOT NULL,
    name VARCHAR(255) NOT NULL,
    path VARCHAR(4096) NOT NULL,
    path_hash VARCHAR(64) AS (SHA2(path, 256)) STORED,
    file_id VARCHAR(36),
    is_dir BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_ws_path (workspace_id, path_hash),
    INDEX idx_parent (workspace_id, parent_path(255)),
    INDEX idx_ws_path_prefix (workspace_id, path(255))
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_files (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    mime_type VARCHAR(128) DEFAULT 'text/plain',
    storage_type VARCHAR(16) NOT NULL DEFAULT 'inline',
    source_type VARCHAR(16) NOT NULL DEFAULT 'text',
    line_count INT,
    checksum_sha256 VARCHAR(64) NOT NULL,
    revision INT NOT NULL DEFAULT 1,
    ref_count INT NOT NULL DEFAULT 1,
    last_embedded_content_hash VARCHAR(64) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    INDEX idx_workspace (workspace_id),
    INDEX idx_checksum (workspace_id, checksum_sha256)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_file_contents (
    file_id VARCHAR(36) PRIMARY KEY,
    content LONGTEXT NOT NULL
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_file_blobs (
    file_id VARCHAR(36) PRIMARY KEY,
    data LONGBLOB NOT NULL
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_file_chunks (
    file_id VARCHAR(36) NOT NULL,
    chunk_index INT NOT NULL,
    start_line INT NOT NULL,
    line_count INT NOT NULL,
    byte_len INT NOT NULL,
    chunk_sha256 VARCHAR(64) NOT NULL,
    content LONGTEXT NOT NULL,
    PRIMARY KEY (file_id, chunk_index),
    INDEX idx_line_lookup (file_id, start_line)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_outbox (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    event_type VARCHAR(32) NOT NULL,
    payload JSON NOT NULL,
    status VARCHAR(16) DEFAULT 'pending',
    retry_count INT NOT NULL DEFAULT 0,
    max_retries INT NOT NULL DEFAULT 5,
    available_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    lease_until TIMESTAMP NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_claim (status, available_at),
    INDEX idx_retention (status, created_at)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_fs_events (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    event_type VARCHAR(16) NOT NULL,
    path VARCHAR(4096) NOT NULL,
    file_id VARCHAR(36),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ws_poll (workspace_id, id),
    INDEX idx_created_at (created_at)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_accounts (
    id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(128) NOT NULL,
    email VARCHAR(256),
    password_hash VARCHAR(255),
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_email (email)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_api_keys (
    id VARCHAR(36) PRIMARY KEY,
    account_id VARCHAR(36) NOT NULL,
    name VARCHAR(128) NOT NULL,
    key_hash VARCHAR(64) NOT NULL,
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_key_hash (key_hash),
    INDEX idx_account (account_id)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_workspaces (
    id VARCHAR(36) PRIMARY KEY,
    account_id VARCHAR(36) NOT NULL,
    name VARCHAR(128) NOT NULL,
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_account_name (account_id, name)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_workspace_keys (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    name VARCHAR(128) NOT NULL,
    key_hash VARCHAR(64) NOT NULL,
    permission VARCHAR(16) DEFAULT 'readwrite',
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_key_hash (key_hash),
    INDEX idx_workspace (workspace_id)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_collection_schemas (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    name VARCHAR(128) NOT NULL,
    collection_type VARCHAR(16) NOT NULL DEFAULT 'structured',
    schema_json JSON NOT NULL,
    embedding_source VARCHAR(128),
    embedding_dim INT,
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_ws_name (workspace_id, name)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_summaries (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    file_id VARCHAR(36),
    dentry_id VARCHAR(36),
    l0_abstract TEXT NOT NULL,
    l1_overview TEXT NOT NULL,
    status VARCHAR(16) DEFAULT 'pending',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_file (file_id),
    UNIQUE INDEX idx_dentry (dentry_id),
    INDEX idx_workspace (workspace_id)
)"#,
            r#"CREATE TABLE IF NOT EXISTS veda_datasets (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    name VARCHAR(64) NOT NULL,
    status VARCHAR(16) DEFAULT 'active',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_ws_name (workspace_id, name),
    INDEX idx_workspace (workspace_id)
)"#,
        ];
        for s in stmts {
            sqlx::query(s)
                .execute(&self.pool)
                .await
                .map_err(|e| VedaError::Storage(e.to_string()))?;
        }
        // Idempotent ALTERs for upgrading existing schemas. MySQL has no
        // ADD COLUMN IF NOT EXISTS pre-8.0.29 — swallow duplicate errors
        // (1060 column, 1061 index). sqlx's `code()` returns the SQLSTATE
        // (e.g. "42S21"), not the MySQL-specific error number, so we
        // downcast to MySqlDatabaseError and check `number()`.
        let alters = [
            "ALTER TABLE veda_workspaces ADD COLUMN kind VARCHAR(16) NOT NULL DEFAULT 'fs'",
            "ALTER TABLE veda_workspaces ADD COLUMN app_id VARCHAR(64) NULL",
            "ALTER TABLE veda_workspaces ADD INDEX idx_app (app_id)",
            "ALTER TABLE veda_workspaces ADD COLUMN description TEXT NULL",
            "ALTER TABLE veda_datasets ADD COLUMN description TEXT NULL",
            "ALTER TABLE veda_api_keys ADD COLUMN app_id VARCHAR(64) NULL",
            "ALTER TABLE veda_api_keys ADD COLUMN allowed_workspaces JSON NULL",
            "ALTER TABLE veda_api_keys ADD COLUMN expires_at DATETIME NULL",
            "ALTER TABLE veda_api_keys ADD INDEX idx_app (app_id)",
            "ALTER TABLE veda_accounts ADD COLUMN app_id VARCHAR(64) NULL",
            "ALTER TABLE veda_accounts ADD UNIQUE INDEX idx_account_app (app_id)",
            // Denormalized onto the key so wk_ auth is one query (JOIN
            // accounts) instead of key-JOIN-workspace-JOIN-account + a
            // second get_workspace. account_id default '' is the
            // "needs backfill" sentinel; new keys insert the real value.
            "ALTER TABLE veda_workspace_keys ADD COLUMN kind VARCHAR(16) NOT NULL DEFAULT 'fs'",
            "ALTER TABLE veda_workspace_keys ADD COLUMN account_id VARCHAR(36) NOT NULL DEFAULT ''",
            // Lease ownership for workers on multiple servers sharing one
            // MySQL: claim stamps the claimant's identity (host:pid) and
            // complete/fail/renew are fenced on it, so an executor whose
            // lease expired (and was re-claimed elsewhere) cannot overwrite
            // the new owner's task state.
            "ALTER TABLE veda_outbox ADD COLUMN lease_owner VARCHAR(128) NULL",
            // Outbox dedup (try_insert_outbox_for_file / has_pending_event)
            // filters on these three equalities before its JSON_EXTRACT;
            // without the index it scans the whole pending backlog inside
            // the write transaction.
            "ALTER TABLE veda_outbox ADD INDEX idx_dedup (workspace_id, event_type, status)",
            // Search-hit path backfill resolves dentries by file_id; the
            // existing indexes all lead with path columns, so this lookup
            // otherwise scans every dentry in the workspace.
            "ALTER TABLE veda_dentries ADD INDEX idx_ws_file (workspace_id, file_id)",
            // Platform (AI Workbench / apps surface) columns — all nullable so
            // direct (non-gateway) access is unaffected. creator/creator_name are
            // stamped from the gateway `user` header (item 2). `token` stores the
            // plaintext wk_ so the console can re-reveal it via getToken (item 1);
            // only populated for keys minted on the apps surface.
            "ALTER TABLE veda_workspaces ADD COLUMN creator VARCHAR(64) NULL",
            "ALTER TABLE veda_workspaces ADD COLUMN creator_name VARCHAR(128) NULL",
            "ALTER TABLE veda_datasets ADD COLUMN creator VARCHAR(64) NULL",
            "ALTER TABLE veda_datasets ADD COLUMN creator_name VARCHAR(128) NULL",
            "ALTER TABLE veda_workspace_keys ADD COLUMN creator VARCHAR(64) NULL",
            "ALTER TABLE veda_workspace_keys ADD COLUMN creator_name VARCHAR(128) NULL",
            "ALTER TABLE veda_workspace_keys ADD COLUMN token VARCHAR(128) NULL",
        ];
        for s in alters {
            if let Err(e) = sqlx::query(s).execute(&self.pool).await {
                let is_dup = matches!(&e, sqlx::Error::Database(db)
                    if db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                        .map(|me| matches!(me.number(), 1060 | 1061))
                        .unwrap_or(false));
                if !is_dup {
                    return Err(VedaError::Storage(e.to_string()));
                }
            }
        }

        // Backfill kind/account_id onto pre-existing workspace keys (the
        // columns added above default to 'fs'/''). Idempotent: only rows
        // still at the '' sentinel are touched, so re-running each boot is a
        // no-op once filled. New keys are inserted with correct values.
        sqlx::query(
            r#"UPDATE veda_workspace_keys k
               JOIN veda_workspaces w ON w.id = k.workspace_id
               SET k.account_id = w.account_id, k.kind = w.kind
               WHERE k.account_id = ''"#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| VedaError::Storage(e.to_string()))?;

        // One-time widen of veda_file_chunks.content from the original
        // MEDIUMTEXT (16 MB) to LONGTEXT, matching veda_file_contents. A
        // single chunk can reach the 50 MB file cap when a >16 MB span has
        // no newline (split_and_hash extends the chunk to the next '\n'/EOF
        // with no byte cap), overflowing MEDIUMTEXT and failing the INSERT
        // under strict sql_mode. Guarded by information_schema so the
        // table-rebuilding MODIFY runs only on a not-yet-migrated schema,
        // not on every boot.
        // Select a literal `1`, NOT `DATA_TYPE`: MySQL information_schema
        // text columns are backed by binary/LONGBLOB, which sqlx refuses to
        // decode into `String`. We only need the row's existence (the
        // `DATA_TYPE = 'mediumtext'` predicate runs server-side), so never
        // pull the text column over the wire.
        let chunk_content_mediumtext: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'veda_file_chunks' \
               AND COLUMN_NAME = 'content' AND DATA_TYPE = 'mediumtext'",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VedaError::Storage(e.to_string()))?;
        if chunk_content_mediumtext.is_some() {
            sqlx::query("ALTER TABLE veda_file_chunks MODIFY content LONGTEXT NOT NULL")
                .execute(&self.pool)
                .await
                .map_err(|e| VedaError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }
}

async fn get_dentry_conn(
    conn: &mut sqlx::MySqlConnection,
    workspace_id: &str,
    path: &str,
) -> Result<Option<Dentry>> {
    let row = sqlx::query(
        r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
           FROM veda_dentries WHERE workspace_id = ? AND path = ?"#,
    )
    .bind(workspace_id)
    .bind(path)
    .fetch_optional(conn)
    .await
    .map_err(storage_err)?;
    row.map(|r| row_to_dentry(&r)).transpose()
}

async fn get_file_conn(
    conn: &mut sqlx::MySqlConnection,
    file_id: &str,
) -> Result<Option<FileRecord>> {
    let row = sqlx::query(
        r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
                  checksum_sha256, revision, ref_count, last_embedded_content_hash,
                  created_at, updated_at
           FROM veda_files WHERE id = ?"#,
    )
    .bind(file_id)
    .fetch_optional(conn)
    .await
    .map_err(storage_err)?;
    row.map(|r| row_to_file(&r)).transpose()
}

async fn list_dentries_under_page_conn(
    conn: &mut sqlx::MySqlConnection,
    workspace_id: &str,
    path_prefix: &str,
    after_path: Option<&str>,
    limit: usize,
) -> Result<Vec<Dentry>> {
    // Empty string sorts before every real path (paths begin with '/'),
    // so `path > ''` is equivalent to "no cursor". This collapses the
    // None / Some(..) split to a single SQL statement.
    let after = after_path.unwrap_or("");
    let limit_i64 = limit as i64;
    let rows = if path_prefix == "/" {
        sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries
               WHERE workspace_id = ? AND path > ?
               ORDER BY path
               LIMIT ?"#,
        )
        .bind(workspace_id)
        .bind(after)
        .bind(limit_i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage_err)?
    } else {
        let like = format!("{}/%", escape_like(path_prefix));
        sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries
               WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\' AND path > ?
               ORDER BY path
               LIMIT ?"#,
        )
        .bind(workspace_id)
        .bind(&like)
        .bind(after)
        .bind(limit_i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage_err)?
    };
    rows.iter().map(|r| row_to_dentry(r)).collect()
}

async fn get_file_chunks_conn(
    conn: &mut sqlx::MySqlConnection,
    file_id: &str,
    start_line: Option<i32>,
    end_line: Option<i32>,
) -> Result<Vec<FileChunk>> {
    let mut q = String::from(
        r#"SELECT file_id, chunk_index, start_line, line_count, byte_len, chunk_sha256, content FROM veda_file_chunks WHERE file_id = ?"#,
    );
    let rows = match (start_line, end_line) {
        (Some(a), Some(b)) => {
            // The inner subquery picks the chunk that *starts at or before* line a
            // (highest chunk_index with start_line <= a). When `a` is past EOF, this
            // still returns the LAST chunk — which doesn't actually contain line a.
            // The outer `start_line + line_count >= ?` filter excludes chunks whose
            // line range ends before a. We use `>=` (not `>`) because line_count is
            // the count of '\n' bytes inside the chunk: when the file has no
            // trailing newline, the final logical line satisfies
            // `start_line + line_count == last_line`, and `>` would drop it.
            q.push_str(
                " AND chunk_index >= COALESCE((SELECT MAX(chunk_index) \
                   FROM veda_file_chunks WHERE file_id = ? AND start_line <= ?), 0) \
                   AND start_line + line_count >= ? \
                   AND start_line <= ? ORDER BY chunk_index",
            );
            sqlx::query(&q)
                .bind(file_id)
                .bind(file_id)
                .bind(a)
                .bind(a)
                .bind(b)
                .fetch_all(&mut *conn)
                .await
        }
        (Some(a), None) => {
            q.push_str(
                " AND chunk_index >= COALESCE((SELECT MAX(chunk_index) \
                   FROM veda_file_chunks WHERE file_id = ? AND start_line <= ?), 0) \
                   AND start_line + line_count >= ? \
                   ORDER BY chunk_index",
            );
            sqlx::query(&q)
                .bind(file_id)
                .bind(file_id)
                .bind(a)
                .bind(a)
                .fetch_all(&mut *conn)
                .await
        }
        (None, Some(b)) => {
            q.push_str(" AND start_line <= ? ORDER BY chunk_index");
            sqlx::query(&q)
                .bind(file_id)
                .bind(b)
                .fetch_all(&mut *conn)
                .await
        }
        (None, None) => {
            q.push_str(" ORDER BY chunk_index");
            sqlx::query(&q).bind(file_id).fetch_all(&mut *conn).await
        }
    }
    .map_err(storage_err)?;
    rows.iter().map(|r| row_to_file_chunk(r)).collect()
}

async fn insert_fs_event_conn(conn: &mut sqlx::MySqlConnection, event: &FsEvent) -> Result<()> {
    let et = db_enum_str(&event.event_type);
    if event.id == 0 {
        sqlx::query(
            r#"INSERT INTO veda_fs_events (workspace_id, event_type, path, file_id, created_at)
               VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&event.path)
        .bind(&event.file_id)
        .bind(event.created_at.naive_utc())
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    } else {
        sqlx::query(
            r#"INSERT INTO veda_fs_events (id, workspace_id, event_type, path, file_id, created_at)
               VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(event.id)
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&event.path)
        .bind(&event.file_id)
        .bind(event.created_at.naive_utc())
        .execute(&mut *conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

async fn insert_outbox_conn(conn: &mut sqlx::MySqlConnection, event: &OutboxEvent) -> Result<()> {
    use chrono::Timelike;
    let payload = serde_json::to_string(&event.payload).map_err(|e| storage_err(e.to_string()))?;
    let status = db_enum_str(&event.status);
    let et = db_enum_str(&event.event_type);
    // MySQL ROUNDS fractional seconds into TIMESTAMP(0): an available_at of
    // ...39.7 lands as ...40, putting a just-enqueued task up to 500ms in the
    // future where claim's `available_at <= UTC_TIMESTAMP()` can't see it.
    // Truncate so enqueue-then-claim is immediate.
    let available_at = event
        .available_at
        .naive_utc()
        .with_nanosecond(0)
        .expect("nanosecond 0 is always valid");
    if event.id == 0 {
        sqlx::query(
            r#"INSERT INTO veda_outbox
            (workspace_id, event_type, payload, status, retry_count, max_retries, available_at, lease_until, created_at)
            VALUES (?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&payload)
        .bind(status)
        .bind(event.retry_count)
        .bind(event.max_retries)
        .bind(available_at)
        .bind(event.lease_until.map(|x| x.naive_utc()))
        .bind(event.created_at.naive_utc())
        .execute(conn)
        .await
        .map_err(storage_err)?;
    } else {
        sqlx::query(
            r#"INSERT INTO veda_outbox
            (id, workspace_id, event_type, payload, status, retry_count, max_retries, available_at, lease_until, created_at)
            VALUES (?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(event.id)
        .bind(&event.workspace_id)
        .bind(et)
        .bind(&payload)
        .bind(status)
        .bind(event.retry_count)
        .bind(event.max_retries)
        .bind(available_at)
        .bind(event.lease_until.map(|x| x.naive_utc()))
        .bind(event.created_at.naive_utc())
        .execute(conn)
        .await
        .map_err(storage_err)?;
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for MysqlStore {
    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
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
    ) -> Result<std::collections::HashMap<String, String>> {
        if file_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = vec!["?"; file_ids.len()].join(",");
        let sql = format!(
            "SELECT file_id, path FROM veda_dentries \
             WHERE workspace_id = ? AND file_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(workspace_id);
        for id in file_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(storage_err)?;
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let fid: String = r.try_get("file_id").map_err(storage_err)?;
            let path: String = r.try_get("path").map_err(storage_err)?;
            map.entry(fid).or_insert(path);
        }
        Ok(map)
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

fn row_to_summary(row: &sqlx::mysql::MySqlRow) -> Result<FileSummary> {
    let status_str: String = row.try_get("status").map_err(storage_err)?;
    let status: SummaryStatus = db_enum("summary_status", &status_str)?;
    Ok(FileSummary {
        id: row.try_get("id").map_err(storage_err)?,
        workspace_id: row.try_get("workspace_id").map_err(storage_err)?,
        file_id: row.try_get("file_id").map_err(storage_err)?,
        dentry_id: row.try_get("dentry_id").map_err(storage_err)?,
        l0_abstract: row.try_get("l0_abstract").map_err(storage_err)?,
        l1_overview: row.try_get("l1_overview").map_err(storage_err)?,
        status,
        created_at: row.try_get("created_at").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
    })
}

pub struct MysqlMetadataTx {
    tx: Option<Transaction<'static, sqlx::MySql>>,
}

impl MysqlMetadataTx {
    fn tx_mut(&mut self) -> Result<&mut Transaction<'static, sqlx::MySql>> {
        self.tx
            .as_mut()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))
    }
}

#[async_trait]
impl MetadataTx for MysqlMetadataTx {
    async fn get_dentry(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>> {
        let t = self.tx_mut()?;
        let row = sqlx::query(
            r#"SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
               FROM veda_dentries WHERE workspace_id = ? AND path = ? FOR UPDATE"#,
        )
        .bind(workspace_id)
        .bind(path)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_dentry(&r)).transpose()
    }

    async fn insert_dentry(&mut self, dentry: &Dentry) -> Result<()> {
        let t = self.tx_mut()?;
        match sqlx::query(
            r#"INSERT INTO veda_dentries
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
        .execute(t.as_mut())
        .await
        {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(ref db_err))
                if db_err
                    .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                    .is_some_and(|e| e.number() == 1062) =>
            {
                Err(VedaError::AlreadyExists(format!("dentry {}", dentry.path)))
            }
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn update_dentry_file_id(
        &mut self,
        workspace_id: &str,
        path: &str,
        file_id: &str,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"UPDATE veda_dentries SET file_id = ? WHERE workspace_id = ? AND path = ?"#)
            .bind(file_id)
            .bind(workspace_id)
            .bind(path)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_dentry(&mut self, workspace_id: &str, path: &str) -> Result<u64> {
        let t = self.tx_mut()?;
        let r = sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path = ?"#)
            .bind(workspace_id)
            .bind(path)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn list_dentries_under_page(
        &mut self,
        workspace_id: &str,
        path_prefix: &str,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Dentry>> {
        let t = self.tx_mut()?;
        list_dentries_under_page_conn(t.as_mut(), workspace_id, path_prefix, after_path, limit)
            .await
    }

    async fn delete_dentries_under(
        &mut self,
        workspace_id: &str,
        parent_path: &str,
    ) -> Result<u64> {
        let t = self.tx_mut()?;
        let r = if parent_path == "/" {
            sqlx::query(
                r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path <> '/' AND path LIKE '/%'"#,
            )
            .bind(workspace_id)
            .execute(t.as_mut())
            .await
        } else {
            let like = format!("{}/%", escape_like(parent_path));
            sqlx::query(r#"DELETE FROM veda_dentries WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\'"#)
                .bind(workspace_id)
                .bind(like)
                .execute(t.as_mut())
                .await
        }
        .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn rename_dentry(
        &mut self,
        workspace_id: &str,
        old_path: &str,
        new_path: &str,
        new_parent: &str,
        new_name: &str,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"UPDATE veda_dentries SET path = ?, parent_path = ?, name = ?
               WHERE workspace_id = ? AND path = ?"#,
        )
        .bind(new_path)
        .bind(new_parent)
        .bind(new_name)
        .bind(workspace_id)
        .bind(old_path)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn rename_dentries_under(
        &mut self,
        workspace_id: &str,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<u64> {
        let t = self.tx_mut()?;
        // CHAR_LENGTH (not Rust's byte len) — MySQL SUBSTRING on VARCHAR uses
        // character offsets, so Unicode paths need character-based slicing.
        let like = format!("{}/%", escape_like(old_prefix));
        let r = sqlx::query(
            r#"UPDATE veda_dentries
               SET path = CONCAT(?, SUBSTRING(path, CHAR_LENGTH(?) + 1)),
                   parent_path = CONCAT(?, SUBSTRING(parent_path, CHAR_LENGTH(?) + 1))
               WHERE workspace_id = ? AND path LIKE ? ESCAPE '\\'"#,
        )
        .bind(new_prefix)
        .bind(old_prefix)
        .bind(new_prefix)
        .bind(old_prefix)
        .bind(workspace_id)
        .bind(&like)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(r.rows_affected())
    }

    async fn get_file(&mut self, file_id: &str) -> Result<Option<FileRecord>> {
        let t = self.tx_mut()?;
        let row = sqlx::query(
            r#"SELECT id, workspace_id, size_bytes, mime_type, storage_type, source_type,
                      line_count, checksum_sha256, revision, ref_count,
                      last_embedded_content_hash, created_at, updated_at
               FROM veda_files WHERE id = ? FOR UPDATE"#,
        )
        .bind(file_id)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_file(&r)).transpose()
    }

    async fn insert_file(&mut self, file: &FileRecord) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_files
            (id, workspace_id, size_bytes, mime_type, storage_type, source_type, line_count,
             checksum_sha256, revision, ref_count, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&file.id)
        .bind(&file.workspace_id)
        .bind(file.size_bytes)
        .bind(&file.mime_type)
        .bind(db_enum_str(&file.storage_type))
        .bind(db_enum_str(&file.source_type))
        .bind(file.line_count)
        .bind(&file.checksum_sha256)
        .bind(file.revision)
        .bind(file.ref_count)
        .bind(file.created_at.naive_utc())
        .bind(file.updated_at.naive_utc())
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn update_file_revision(
        &mut self,
        file_id: &str,
        expected_rev: i32,
        new_rev: i32,
        size_bytes: i64,
        checksum: &str,
        line_count: Option<i32>,
        storage_type: StorageType,
        mime_type: &str,
        source_type: SourceType,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        let r = sqlx::query(
            r#"UPDATE veda_files
               SET revision = ?, size_bytes = ?, checksum_sha256 = ?, line_count = ?, storage_type = ?, mime_type = ?, source_type = ?, last_embedded_content_hash = NULL
               WHERE id = ? AND revision = ?"#,
        )
        .bind(new_rev)
        .bind(size_bytes)
        .bind(checksum)
        .bind(line_count)
        .bind(db_enum_str(&storage_type))
        .bind(mime_type)
        .bind(db_enum_str(&source_type))
        .bind(file_id)
        .bind(expected_rev)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        if r.rows_affected() == 0 {
            return Err(VedaError::PreconditionFailed(format!(
                "file {file_id} revision mismatch (expected {expected_rev})"
            )));
        }
        Ok(())
    }

    async fn decrement_ref_count(&mut self, file_id: &str) -> Result<i32> {
        let t = self.tx_mut()?;
        let row = sqlx::query(r#"SELECT ref_count FROM veda_files WHERE id = ? FOR UPDATE"#)
            .bind(file_id)
            .fetch_optional(t.as_mut())
            .await
            .map_err(storage_err)?;
        let r = row.ok_or_else(|| VedaError::NotFound(file_id.to_string()))?;
        let current: i32 = r.try_get("ref_count").map_err(storage_err)?;
        if current <= 0 {
            return Err(VedaError::Internal(format!(
                "ref_count already {} for file {file_id}",
                current
            )));
        }
        let new_count = current - 1;
        sqlx::query(r#"UPDATE veda_files SET ref_count = ? WHERE id = ?"#)
            .bind(new_count)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(new_count)
    }

    async fn increment_ref_count(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"UPDATE veda_files SET ref_count = ref_count + 1 WHERE id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_files WHERE id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn get_file_content(&mut self, file_id: &str) -> Result<Option<String>> {
        let t = self.tx_mut()?;
        let row = sqlx::query(r#"SELECT content FROM veda_file_contents WHERE file_id = ?"#)
            .bind(file_id)
            .fetch_optional(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(row
            .map(|r| r.try_get::<String, _>("content"))
            .transpose()
            .map_err(storage_err)?)
    }

    async fn get_file_chunks(
        &mut self,
        file_id: &str,
        start_line: Option<i32>,
        end_line: Option<i32>,
    ) -> Result<Vec<FileChunk>> {
        let t = self.tx_mut()?;
        get_file_chunks_conn(t.as_mut(), file_id, start_line, end_line).await
    }

    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_file_contents (file_id, content) VALUES (?, ?)
               ON DUPLICATE KEY UPDATE content = VALUES(content)"#,
        )
        .bind(file_id)
        .bind(content)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_content(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_contents WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_file_blob(&mut self, file_id: &str, data: &[u8]) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(
            r#"INSERT INTO veda_file_blobs (file_id, data) VALUES (?, ?)
               ON DUPLICATE KEY UPDATE data = VALUES(data)"#,
        )
        .bind(file_id)
        .bind(data)
        .execute(t.as_mut())
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_blob(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_blobs WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_file_chunks(&mut self, chunks: &[FileChunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let t = self.tx_mut()?;
        for batch in chunks.chunks(CHUNK_INSERT_BATCH) {
            let placeholders: Vec<&str> = batch.iter().map(|_| "(?, ?, ?, ?, ?, ?, ?)").collect();
            let sql = format!(
                "INSERT INTO veda_file_chunks \
                 (file_id, chunk_index, start_line, line_count, byte_len, chunk_sha256, content) \
                 VALUES {}",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for c in batch {
                q = q
                    .bind(&c.file_id)
                    .bind(c.chunk_index)
                    .bind(c.start_line)
                    .bind(c.line_count)
                    .bind(c.byte_len)
                    .bind(&c.chunk_sha256)
                    .bind(&c.content);
            }
            q.execute(t.as_mut()).await.map_err(storage_err)?;
        }
        Ok(())
    }

    async fn delete_file_chunks(&mut self, file_id: &str) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_chunks WHERE file_id = ?"#)
            .bind(file_id)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_file_chunks_from(
        &mut self,
        file_id: &str,
        from_chunk_index: i32,
    ) -> Result<()> {
        let t = self.tx_mut()?;
        sqlx::query(r#"DELETE FROM veda_file_chunks WHERE file_id = ? AND chunk_index >= ?"#)
            .bind(file_id)
            .bind(from_chunk_index)
            .execute(t.as_mut())
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn insert_outbox(&mut self, event: &OutboxEvent) -> Result<()> {
        let t = self.tx_mut()?;
        insert_outbox_conn(t.as_mut(), event).await
    }

    async fn try_insert_outbox_for_file(
        &mut self,
        event: &OutboxEvent,
        file_id: &str,
    ) -> Result<bool> {
        let t = self.tx_mut()?;
        let et = db_enum_str(&event.event_type);
        // Only deduplicate against `pending` events. A `processing` event is
        // already in flight against an older snapshot of the file; if we
        // dedupe against it, the new content's ChunkSync gets dropped and the
        // worker silently embeds stale data. Letting it through means after
        // the in-flight task completes, the worker picks up the new pending
        // event; if the second event's content_hash equals the watermark by
        // then (e.g. user reverted), `handle_chunk_sync` short-circuits.
        let row: Option<(i64,)> = sqlx::query_as(
            r#"SELECT 1 FROM veda_outbox
               WHERE event_type = ? AND workspace_id = ? AND status = 'pending'
                 AND JSON_UNQUOTE(JSON_EXTRACT(payload, '$.file_id')) = ?
               LIMIT 1"#,
        )
        .bind(et)
        .bind(&event.workspace_id)
        .bind(file_id)
        .fetch_optional(t.as_mut())
        .await
        .map_err(storage_err)?;
        if row.is_some() {
            return Ok(false);
        }
        insert_outbox_conn(t.as_mut(), event).await?;
        Ok(true)
    }

    async fn insert_fs_event(&mut self, event: &FsEvent) -> Result<()> {
        let t = self.tx_mut()?;
        insert_fs_event_conn(t.as_mut(), event).await
    }

    async fn insert_fs_events(&mut self, events: &[FsEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let t = self.tx_mut()?;
        for batch in events.chunks(FS_EVENT_INSERT_BATCH) {
            let placeholders: Vec<&str> = batch.iter().map(|_| "(?, ?, ?, ?, ?)").collect();
            let sql = format!(
                "INSERT INTO veda_fs_events (workspace_id, event_type, path, file_id, created_at) VALUES {}",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for e in batch {
                q = q
                    .bind(&e.workspace_id)
                    .bind(db_enum_str(&e.event_type))
                    .bind(&e.path)
                    .bind(&e.file_id)
                    .bind(e.created_at.naive_utc());
            }
            q.execute(t.as_mut()).await.map_err(storage_err)?;
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        let tx = self
            .tx
            .take()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))?;
        tx.commit().await.map_err(storage_err)?;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<()> {
        let tx = self
            .tx
            .take()
            .ok_or_else(|| VedaError::Storage("transaction already finished".into()))?;
        tx.rollback().await.map_err(storage_err)?;
        Ok(())
    }
}

/// Outbox lease duration. Workers heartbeat-renew at a fraction of this
/// (veda-server `LEASE_RENEW_INTERVAL`), so a lease only expires when its
/// owner stopped renewing for the whole window — i.e. crashed or was
/// SIGKILLed — after which any claimer may take the row over.
const OUTBOX_LEASE_MINUTES: i32 = 10;

#[async_trait]
impl TaskQueue for MysqlStore {
    async fn enqueue(&self, event: &OutboxEvent) -> Result<()> {
        let mut conn = self.pool.acquire().await.map_err(storage_err)?;
        insert_outbox_conn(&mut *conn, event).await
    }

    async fn claim(&self, owner: &str, batch_size: usize) -> Result<Vec<OutboxEvent>> {
        let batch_size_i64 = i64::try_from(batch_size).unwrap_or(100);
        let mut tx = self.pool.begin().await.map_err(storage_err)?;
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, event_type, payload, status, retry_count, max_retries,
                      available_at, lease_until, lease_owner, created_at
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
        let mut takeovers: Vec<(i64, String, String)> = Vec::new();
        for r in &rows {
            let mut evt = row_to_outbox(r)?;
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
                let prev_owner: Option<String> = r.try_get("lease_owner").map_err(storage_err)?;
                takeovers.push((
                    evt.id,
                    db_enum_str(&evt.event_type),
                    prev_owner.unwrap_or_default(),
                ));
                sqlx::query(
                    r#"UPDATE veda_outbox SET status = 'processing', retry_count = ?,
                       lease_owner = ?, lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE)
                       WHERE id = ?"#,
                )
                .bind(next_retry)
                .bind(owner)
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
                       lease_owner = ?, lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE)
                       WHERE id = ?"#,
                )
                .bind(owner)
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
                r#"UPDATE veda_outbox SET status = 'dead', lease_until = NULL, lease_owner = NULL WHERE id = ?"#,
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
        // A takeover means the previous executor stopped heartbeating (crash /
        // SIGKILL) — or, if it is somehow still alive, its complete/fail will
        // now be fenced off and its side effects (embedding spend) duplicated.
        for (id, event_type, prev_owner) in &takeovers {
            tracing::warn!(
                task_id = *id,
                event_type = %event_type,
                prev_owner = %prev_owner,
                new_owner = %owner,
                "outbox lease taken over from expired owner"
            );
            ::metrics::counter!("veda_outbox_lease_takeover_total", "event_type" => event_type.clone())
                .increment(1);
        }
        Ok(events)
    }

    async fn complete(&self, task_id: i64, owner: &str) -> Result<()> {
        let res = sqlx::query(
            r#"UPDATE veda_outbox SET status = 'completed', lease_until = NULL, lease_owner = NULL
               WHERE id = ? AND lease_owner = ? AND status = 'processing'"#,
        )
        .bind(task_id)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        if res.rows_affected() == 0 {
            // Lease lost mid-flight: it expired and another worker re-claimed
            // the row (which now owns its lifecycle). The work this executor
            // did still happened — the new owner repeats it — so surface the
            // duplicate side effects instead of silently overwriting state.
            tracing::warn!(task_id, owner, "outbox complete dropped: lease no longer held");
            ::metrics::counter!("veda_outbox_lease_lost_total", "op" => "complete").increment(1);
        }
        Ok(())
    }

    async fn fail(&self, task_id: i64, owner: &str, error: &str) -> Result<()> {
        // Owner check up front: if the lease was lost (expired → re-claimed
        // elsewhere) this executor must not touch retry bookkeeping the new
        // owner now drives. The SELECT→UPDATE pair is not transactional; a
        // takeover between the two is caught by the owner condition on the
        // UPDATEs below (rows_affected = 0).
        let row = sqlx::query(
            r#"SELECT id, retry_count, max_retries, payload, event_type FROM veda_outbox
               WHERE id = ? AND lease_owner = ? AND status = 'processing'"#,
        )
        .bind(task_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        let Some(r) = row else {
            tracing::warn!(task_id, owner, "outbox fail dropped: lease no longer held");
            ::metrics::counter!("veda_outbox_lease_lost_total", "op" => "fail").increment(1);
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
            // Owner+status fencing makes the terminal transition idempotent
            // and exclusive: a lease lost between the SELECT above and here
            // (another worker re-claimed, or claim() already dead-lettered
            // the row) leaves rows_affected = 0 and the dead counter exact.
            let res = sqlx::query(
                r#"UPDATE veda_outbox SET status = 'dead', retry_count = ?, payload = CAST(? AS JSON),
                   lease_until = NULL, lease_owner = NULL
                   WHERE id = ? AND lease_owner = ? AND status = 'processing'"#,
            )
            .bind(next_retry)
            .bind(&payload_str)
            .bind(task_id)
            .bind(owner)
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
                tracing::warn!(task_id, owner, "outbox fail dropped: lease no longer held");
                ::metrics::counter!("veda_outbox_lease_lost_total", "op" => "fail").increment(1);
            }
        } else {
            let backoff_secs: i64 = (30 * (1i64 << next_retry.min(10))).min(3600);
            let res = sqlx::query(
                "UPDATE veda_outbox SET status = 'pending', retry_count = ?, payload = CAST(? AS JSON), \
                 available_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? SECOND), lease_until = NULL, \
                 lease_owner = NULL WHERE id = ? AND lease_owner = ? AND status = 'processing'",
            )
                .bind(next_retry)
                .bind(&payload_str)
                .bind(backoff_secs)
                .bind(task_id)
                .bind(owner)
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
            if res.rows_affected() == 0 {
                tracing::warn!(task_id, owner, "outbox fail dropped: lease no longer held");
                ::metrics::counter!("veda_outbox_lease_lost_total", "op" => "fail").increment(1);
            }
        }
        Ok(())
    }

    async fn renew(&self, task_ids: &[i64], owner: &str) -> Result<()> {
        if task_ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; task_ids.len()].join(",");
        let sql = format!(
            "UPDATE veda_outbox SET lease_until = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ? MINUTE) \
             WHERE lease_owner = ? AND status = 'processing' AND id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql).bind(OUTBOX_LEASE_MINUTES).bind(owner);
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

// ── CollectionMetaStore ────────────────────────────────

#[async_trait]
impl CollectionMetaStore for MysqlStore {
    async fn create_collection_schema(&self, schema: &CollectionSchema) -> Result<()> {
        let schema_str =
            serde_json::to_string(&schema.schema_json).map_err(|e| storage_err(e.to_string()))?;
        sqlx::query(
            r#"INSERT INTO veda_collection_schemas
               (id, workspace_id, name, collection_type, schema_json, embedding_source, embedding_dim, status, created_at, updated_at)
               VALUES (?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?, ?, ?)"#,
        )
        .bind(&schema.id)
        .bind(&schema.workspace_id)
        .bind(&schema.name)
        .bind(db_enum_str(&schema.collection_type))
        .bind(&schema_str)
        .bind(&schema.embedding_source)
        .bind(schema.embedding_dim)
        .bind(db_enum_str(&schema.status))
        .bind(schema.created_at.naive_utc())
        .bind(schema.updated_at.naive_utc())
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn get_collection_schema(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<CollectionSchema>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, name, collection_type, schema_json, embedding_source,
                      embedding_dim, status, created_at, updated_at
               FROM veda_collection_schemas WHERE workspace_id = ? AND name = ? AND status = 'active'"#,
        )
        .bind(workspace_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_collection_schema(&r)).transpose()
    }

    async fn get_collection_schema_by_id(&self, id: &str) -> Result<Option<CollectionSchema>> {
        let row = sqlx::query(
            r#"SELECT id, workspace_id, name, collection_type, schema_json, embedding_source,
                      embedding_dim, status, created_at, updated_at
               FROM veda_collection_schemas WHERE id = ?"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;
        row.map(|r| row_to_collection_schema(&r)).transpose()
    }

    async fn list_collection_schemas(&self, workspace_id: &str) -> Result<Vec<CollectionSchema>> {
        let rows = sqlx::query(
            r#"SELECT id, workspace_id, name, collection_type, schema_json, embedding_source,
                      embedding_dim, status, created_at, updated_at
               FROM veda_collection_schemas WHERE workspace_id = ? AND status = 'active' ORDER BY name"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;
        rows.iter().map(|r| row_to_collection_schema(r)).collect()
    }

    async fn delete_collection_schema(&self, id: &str) -> Result<()> {
        sqlx::query(r#"DELETE FROM veda_collection_schemas WHERE id = ?"#)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }
}

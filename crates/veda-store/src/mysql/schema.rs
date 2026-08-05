use super::*;

impl MysqlStore {
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
            r#"CREATE TABLE IF NOT EXISTS veda_file_extracts (
    file_id VARCHAR(36) PRIMARY KEY,
    content LONGTEXT NOT NULL,
    source_sha256 VARCHAR(64) NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
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
            // lease_owner dropped (2026-07 single-pod simplification): the
            // host:pid fencing was multi-server protection this deployment
            // never runs. Lifecycle calls fence on `status='processing'`
            // alone; the content-hash watermark keeps a rare duplicate
            // execution idempotent. Error 1091 (can't drop, doesn't exist)
            // is swallowed below so fresh schemas boot clean.
            "ALTER TABLE veda_outbox DROP COLUMN lease_owner",
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
                // 1060/1061: duplicate column/index (ADD already applied);
                // 1091: can't DROP, doesn't exist (DROP already applied).
                let is_applied = matches!(&e, sqlx::Error::Database(db)
                    if db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                        .map(|me| matches!(me.number(), 1060 | 1061 | 1091))
                        .unwrap_or(false));
                if !is_applied {
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
}

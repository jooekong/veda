use super::*;

pub(super) fn row_to_fs_event(row: &sqlx::mysql::MySqlRow) -> Result<FsEvent> {
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

pub(super) fn row_to_collection_schema(row: &sqlx::mysql::MySqlRow) -> Result<CollectionSchema> {
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

pub(super) fn row_to_account(row: &sqlx::mysql::MySqlRow) -> Result<Account> {
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

pub(super) fn row_to_dataset(row: &sqlx::mysql::MySqlRow) -> Result<Dataset> {
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

pub(super) fn row_to_workspace(row: &sqlx::mysql::MySqlRow) -> Result<Workspace> {
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

pub(super) fn row_to_api_key(row: &sqlx::mysql::MySqlRow) -> Result<ApiKeyRecord> {
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

pub(super) fn row_to_workspace_key(row: &sqlx::mysql::MySqlRow) -> Result<WorkspaceKey> {
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

pub(super) fn row_to_dentry(row: &sqlx::mysql::MySqlRow) -> Result<Dentry> {
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

pub(super) fn row_to_file(row: &sqlx::mysql::MySqlRow) -> Result<FileRecord> {
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

pub(super) fn row_to_file_chunk(row: &sqlx::mysql::MySqlRow) -> Result<FileChunk> {
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

pub(super) fn row_to_outbox(row: &sqlx::mysql::MySqlRow) -> Result<OutboxEvent> {
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


pub(super) fn row_to_summary(row: &sqlx::mysql::MySqlRow) -> Result<FileSummary> {
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


use super::*;

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

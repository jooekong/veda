use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;
use veda_types::*;

use crate::store::{CollectionMetaStore, CollectionVectorStore, EmbeddingService};

pub struct CollectionService {
    meta: Arc<dyn CollectionMetaStore>,
    vector: Arc<dyn CollectionVectorStore>,
    embedding: Arc<dyn EmbeddingService>,
}

impl CollectionService {
    pub fn new(
        meta: Arc<dyn CollectionMetaStore>,
        vector: Arc<dyn CollectionVectorStore>,
        embedding: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            meta,
            vector,
            embedding,
        }
    }

    pub async fn create(
        &self,
        workspace_id: &str,
        name: &str,
        collection_type: CollectionType,
        fields: &[FieldDefinition],
        embed_source: Option<&str>,
    ) -> Result<CollectionSchema> {
        let existing = self.meta.get_collection_schema(workspace_id, name).await?;
        if existing.is_some() {
            return Err(VedaError::AlreadyExists(format!(
                "collection {name} already exists"
            )));
        }

        let embedding_source = embed_source.map(|s| s.to_string());
        let embedding_dim = Some(self.embedding.dimension() as i32);

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let schema_json = serde_json::to_value(fields)
            .map_err(|e| VedaError::Internal(e.to_string()))?;

        let cs = CollectionSchema {
            id: id.clone(),
            workspace_id: workspace_id.to_string(),
            name: name.to_string(),
            collection_type,
            schema_json,
            embedding_source,
            embedding_dim,
            status: CollectionStatus::Active,
            created_at: now,
            updated_at: now,
        };

        let milvus_name = cs.milvus_name();
        self.vector
            .create_dynamic_collection(&milvus_name, fields, self.embedding.dimension() as u32)
            .await?;

        if let Err(e) = self.meta.create_collection_schema(&cs).await {
            let _ = self.vector.drop_dynamic_collection(&milvus_name).await;
            return Err(e);
        }

        Ok(cs)
    }

    pub async fn list(&self, workspace_id: &str) -> Result<Vec<CollectionSchema>> {
        self.meta.list_collection_schemas(workspace_id).await
    }

    pub async fn get(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<CollectionSchema> {
        self.meta
            .get_collection_schema(workspace_id, name)
            .await?
            .ok_or_else(|| VedaError::NotFound(format!("collection {name}")))
    }

    pub async fn delete(&self, workspace_id: &str, name: &str) -> Result<()> {
        let schema = self.get(workspace_id, name).await?;
        self.vector.drop_dynamic_collection(&schema.milvus_name()).await?;
        self.meta.delete_collection_schema(&schema.id).await?;
        Ok(())
    }

    /// Insert rows into a structured collection.
    /// For each row, if there is an embed source field, call the embedding API synchronously.
    pub async fn insert_rows(
        &self,
        workspace_id: &str,
        name: &str,
        rows: &[serde_json::Value],
    ) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let schema = self.get(workspace_id, name).await?;
        let milvus_name = schema.milvus_name();

        let texts: Vec<String> = rows
            .iter()
            .map(|r| {
                if let Some(ref src) = schema.embedding_source {
                    r.get(src)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    serde_json::to_string(r).unwrap_or_default()
                }
            })
            .collect();

        let embeddings = self.embedding.embed(&texts).await?;

        let mut milvus_rows = Vec::with_capacity(rows.len());
        for (row, emb) in rows.iter().zip(embeddings.iter()) {
            let mut r = row.clone();
            if let Some(obj) = r.as_object_mut() {
                obj.entry("id")
                    .or_insert_with(|| serde_json::Value::String(Uuid::new_v4().to_string()));
                obj.insert("vector".to_string(), serde_json::json!(emb));
            }
            milvus_rows.push(r);
        }

        self.vector
            .insert_collection_rows(&milvus_name, workspace_id, &milvus_rows)
            .await?;

        Ok(rows.len())
    }

    pub async fn search(
        &self,
        workspace_id: &str,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let schema = self.get(workspace_id, name).await?;
        let milvus_name = schema.milvus_name();

        let vectors = self.embedding.embed(&[query.to_string()]).await?;
        let vector = vectors.into_iter().next().ok_or_else(|| {
            VedaError::EmbeddingFailed("empty embedding result".to_string())
        })?;

        let limit = if limit == 0 { 10 } else { limit };
        self.vector
            .search_collection(&milvus_name, workspace_id, &vector, limit)
            .await
    }
}

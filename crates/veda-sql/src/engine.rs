use std::sync::Arc;

use arrow::array::RecordBatch;
use datafusion::prelude::*;
use veda_core::store::{CollectionMetaStore, CollectionVectorStore, EmbeddingService, MetadataStore};
use veda_types::VedaError;

use crate::collection_table::CollectionTable;
use crate::files_table::FilesTable;

pub struct VedaSqlEngine {
    meta: Arc<dyn MetadataStore>,
    coll_meta: Arc<dyn CollectionMetaStore>,
    coll_vector: Arc<dyn CollectionVectorStore>,
    #[allow(dead_code)]
    embedding: Arc<dyn EmbeddingService>,
}

impl VedaSqlEngine {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        coll_meta: Arc<dyn CollectionMetaStore>,
        coll_vector: Arc<dyn CollectionVectorStore>,
        embedding: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self { meta, coll_meta, coll_vector, embedding }
    }

    /// Execute a SQL query scoped to a workspace. Automatically registers:
    ///   - `files`: all dentries in the workspace
    ///   - any collection tables the workspace owns
    pub async fn execute(
        &self,
        workspace_id: &str,
        sql: &str,
    ) -> veda_types::Result<Vec<RecordBatch>> {
        let ctx = SessionContext::new();

        let files = FilesTable::new(self.meta.clone(), workspace_id.to_string());
        ctx.register_table("files", Arc::new(files))
            .map_err(|e| VedaError::Storage(e.to_string()))?;

        let schemas = self.coll_meta.list_collection_schemas(workspace_id).await?;
        for schema in &schemas {
            let table = CollectionTable::new(
                self.coll_vector.clone(),
                workspace_id.to_string(),
                schema.clone(),
            );
            ctx.register_table(&schema.name, Arc::new(table))
                .map_err(|e| VedaError::Storage(e.to_string()))?;
        }

        let df = ctx.sql(sql).await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        let batches = df.collect().await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(batches)
    }
}

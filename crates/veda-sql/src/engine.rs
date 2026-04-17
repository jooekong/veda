use std::sync::Arc;

use arrow::array::RecordBatch;
use datafusion::logical_expr::ScalarUDF;
use datafusion::prelude::*;
use veda_core::service::fs::FsService;
use veda_core::store::{CollectionMetaStore, CollectionVectorStore, EmbeddingService, MetadataStore, VectorStore};
use veda_types::VedaError;

use crate::collection_table::CollectionTable;
use crate::embedding_udf::EmbeddingUdf;
use crate::files_table::FilesTable;
use crate::fs_udf::{self, FsUdfContext};
use crate::fs_table::VedaFsTableFactory;
use crate::fs_events_table::VedaFsEventsFactory;
use crate::search_table::VedaSearchFactory;
use crate::storage_stats_table::VedaStorageStatsFactory;

pub struct VedaSqlEngine {
    meta: Arc<dyn MetadataStore>,
    vector: Arc<dyn VectorStore>,
    coll_meta: Arc<dyn CollectionMetaStore>,
    coll_vector: Arc<dyn CollectionVectorStore>,
    fs_service: Arc<FsService>,
    embedding: Arc<dyn EmbeddingService>,
}

impl VedaSqlEngine {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        vector: Arc<dyn VectorStore>,
        coll_meta: Arc<dyn CollectionMetaStore>,
        coll_vector: Arc<dyn CollectionVectorStore>,
        embedding: Arc<dyn EmbeddingService>,
        fs_service: Arc<FsService>,
    ) -> Self {
        Self { meta, vector, coll_meta, coll_vector, fs_service, embedding }
    }

    pub async fn execute(
        &self,
        workspace_id: &str,
        read_only: bool,
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

        let fs_ctx = Arc::new(FsUdfContext {
            workspace_id: workspace_id.to_string(),
            fs_service: self.fs_service.clone(),
            read_only,
        });
        fs_udf::register_all(&ctx, fs_ctx);

        ctx.register_udf(ScalarUDF::from(EmbeddingUdf::new(self.embedding.clone())));

        ctx.register_udtf("veda_fs", Arc::new(VedaFsTableFactory {
            workspace_id: workspace_id.to_string(),
            fs_service: self.fs_service.clone(),
        }));

        ctx.register_udtf("veda_fs_events", Arc::new(VedaFsEventsFactory {
            workspace_id: workspace_id.to_string(),
            meta: self.meta.clone(),
        }));

        ctx.register_udtf("veda_storage_stats", Arc::new(VedaStorageStatsFactory {
            workspace_id: workspace_id.to_string(),
            meta: self.meta.clone(),
        }));

        ctx.register_udtf("search", Arc::new(VedaSearchFactory {
            workspace_id: workspace_id.to_string(),
            meta: self.meta.clone(),
            vector: self.vector.clone(),
            embedding: self.embedding.clone(),
        }));

        let df = ctx.sql(sql).await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        let batches = df.collect().await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        Ok(batches)
    }
}

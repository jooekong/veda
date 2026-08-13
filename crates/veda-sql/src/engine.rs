use std::sync::Arc;
use std::time::Duration;

use arrow::array::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::execution::memory_pool::GreedyMemoryPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::logical_expr::ScalarUDF;
use datafusion::prelude::*;
use veda_core::service::fs::FsService;
use veda_core::store::{
    CollectionMetaStore, CollectionVectorStore, EmbeddingService, MetadataStore, VectorStore,
};
use veda_types::VedaError;

use crate::collection_table::CollectionTable;
use crate::embedding_udf::EmbeddingUdf;
use crate::files_table::FilesTable;
use crate::fs_events_table::VedaFsEventsFactory;
use crate::fs_table::VedaFsTableFactory;
use crate::fs_udf::{self, FsUdfContext};
use crate::search_table::VedaSearchFactory;
use crate::storage_stats_table::VedaStorageStatsFactory;

/// Bounded memory for one SQL query. DataFusion's default runtime uses an
/// unbounded pool, so a cross-join / large sort could OOM the whole node;
/// 256 MB is far above any legitimate tenant query and aborts the rest.
const SQL_MEM_POOL_BYTES: usize = 256 * 1024 * 1024;
/// Wall-clock cap on a single SQL query's execution (`collect`). Planning is
/// bounded and cheap for veda's registered-table SELECTs, so it stays outside.
const SQL_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

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
        Self {
            meta,
            vector,
            coll_meta,
            coll_vector,
            fs_service,
            embedding,
        }
    }

    pub async fn execute(
        &self,
        workspace_id: &str,
        read_only: bool,
        sql: &str,
    ) -> veda_types::Result<Vec<RecordBatch>> {
        // Bound query memory so one tenant's cross-join can't OOM the node.
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_pool(Arc::new(GreedyMemoryPool::new(SQL_MEM_POOL_BYTES)))
            .build_arc()
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        let ctx = SessionContext::new_with_config_rt(SessionConfig::new(), runtime);

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

        ctx.register_udtf(
            "veda_fs",
            Arc::new(VedaFsTableFactory {
                workspace_id: workspace_id.to_string(),
                fs_service: self.fs_service.clone(),
            }),
        );

        ctx.register_udtf(
            "veda_fs_events",
            Arc::new(VedaFsEventsFactory {
                workspace_id: workspace_id.to_string(),
                fs_service: self.fs_service.clone(),
            }),
        );

        ctx.register_udtf(
            "veda_storage_stats",
            Arc::new(VedaStorageStatsFactory {
                workspace_id: workspace_id.to_string(),
                meta: self.meta.clone(),
            }),
        );

        ctx.register_udtf(
            "search",
            Arc::new(VedaSearchFactory {
                workspace_id: workspace_id.to_string(),
                meta: self.meta.clone(),
                vector: self.vector.clone(),
                embedding: self.embedding.clone(),
            }),
        );

        // Gate the planner: tenants may only run SELECT (plus `veda_*` write
        // UDFs, which go through Projection + their own read_only check).
        // Blocking DDL/DML/statements kills `COPY ... TO` and `CREATE EXTERNAL
        // TABLE`, which would otherwise read/write arbitrary host files as the
        // server uid. Unconditional — `read_only` governs the UDFs, not the planner.
        let opts = SQLOptions::new()
            .with_allow_ddl(false)
            .with_allow_dml(false)
            .with_allow_statements(false);
        let df = ctx
            .sql_with_options(sql, opts)
            .await
            .map_err(df_error_to_veda)?;
        let batches = tokio::time::timeout(SQL_QUERY_TIMEOUT, df.collect())
            .await
            .map_err(|_| {
                VedaError::Storage(format!(
                    "sql query exceeded {}s",
                    SQL_QUERY_TIMEOUT.as_secs()
                ))
            })?
            .map_err(df_error_to_veda)?;
        Ok(batches)
    }
}

/// Map a DataFusion error to a `VedaError` with the right HTTP status.
///
/// 1. **Recover typed errors**: a UDF/UDTF that surfaced a `VedaError` via
///    `External` (e.g. the read-only write guard → `PermissionDenied`) keeps
///    its real status instead of collapsing to 500. `find_root()` digs through
///    every wrapper variant (`Context`/`Diagnostic`/`Shared`/`Collection`),
///    so recovery is robust to however DataFusion nests the error.
/// 2. **Classify by variant** (not by plan-vs-exec phase): planning/analysis
///    errors — bad SQL, unknown table/column, unsupported feature — are user
///    query errors → `InvalidInput` (4xx). Everything else — crucially the
///    `Execution(String)` that UDTFs (`veda_fs`/`search`) emit when a backend
///    (MySQL/Milvus/embedding) fails — stays `Storage` (5xx), so backend
///    failures are neither mislabeled 4xx nor leaked to the caller.
fn df_error_to_veda(err: DataFusionError) -> VedaError {
    let root = err.find_root();
    if let DataFusionError::External(b) = root {
        if let Some(ve) = b.downcast_ref::<VedaError>() {
            return ve.clone();
        }
    }
    match root {
        DataFusionError::Plan(_)
        | DataFusionError::SchemaError(..)
        | DataFusionError::SQL(..)
        | DataFusionError::NotImplemented(_) => VedaError::InvalidInput(root.to_string()),
        _ => VedaError::Storage(root.to_string()),
    }
}

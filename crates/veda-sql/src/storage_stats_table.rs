use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::catalog::{TableFunctionImpl, TableProvider};
use datafusion::datasource::memory::MemTable;
use datafusion::error::Result;
use datafusion::logical_expr::Expr;

use veda_core::store::MetadataStore;

use crate::fs_udf;

fn stats_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("total_files", DataType::Int64, false),
        Field::new("total_directories", DataType::Int64, false),
        Field::new("total_bytes", DataType::Int64, false),
    ]))
}

pub struct VedaStorageStatsFactory {
    pub workspace_id: String,
    pub meta: Arc<dyn MetadataStore>,
}

impl Debug for VedaStorageStatsFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VedaStorageStatsFactory").finish()
    }
}

impl TableFunctionImpl for VedaStorageStatsFactory {
    fn call(&self, _exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let stats = fs_udf::block_on(self.meta.storage_stats(&self.workspace_id))
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;

        let schema = stats_schema();

        let mut files_b = Int64Builder::with_capacity(1);
        let mut dirs_b = Int64Builder::with_capacity(1);
        let mut bytes_b = Int64Builder::with_capacity(1);

        files_b.append_value(stats.total_files);
        dirs_b.append_value(stats.total_directories);
        bytes_b.append_value(stats.total_bytes);

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(files_b.finish()),
                Arc::new(dirs_b.finish()),
                Arc::new(bytes_b.finish()),
            ],
        )?;

        let table = MemTable::try_new(schema, vec![vec![batch]])?;
        Ok(Arc::new(table))
    }
}

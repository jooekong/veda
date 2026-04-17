use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::catalog::{TableFunctionImpl, TableProvider};
use datafusion::common::ScalarValue;
use datafusion::datasource::memory::MemTable;
use datafusion::error::Result;
use datafusion::logical_expr::Expr;

use veda_core::store::MetadataStore;

use crate::fs_udf;

const DEFAULT_LIMIT: usize = 1000;

fn fs_events_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("event_type", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        Field::new("file_id", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, false),
    ]))
}

pub struct VedaFsEventsFactory {
    pub workspace_id: String,
    pub meta: Arc<dyn MetadataStore>,
}

impl Debug for VedaFsEventsFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VedaFsEventsFactory").finish()
    }
}

impl TableFunctionImpl for VedaFsEventsFactory {
    /// Arguments (all optional, positional):
    ///   1. since_id (Int64) — only return events with id > since_id
    ///   2. path_prefix (Utf8) — filter by path prefix
    ///   3. limit (Int64) — max rows to return
    fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let mut since_id: i64 = 0;
        let mut path_prefix: Option<String> = None;
        let mut limit: usize = DEFAULT_LIMIT;

        fn extract_scalar(expr: &Expr) -> Option<&ScalarValue> {
            match expr {
                Expr::Literal(sv, _) => Some(sv),
                Expr::Alias(a) => extract_scalar(&a.expr),
                _ => None,
            }
        }

        if let Some(sv) = exprs.first().and_then(|e| extract_scalar(e)) {
            if let ScalarValue::Int64(Some(v)) = sv {
                since_id = *v;
            }
        }
        if let Some(sv) = exprs.get(1).and_then(|e| extract_scalar(e)) {
            if let ScalarValue::Utf8(Some(v)) = sv {
                path_prefix = Some(v.clone());
            }
        }
        if let Some(sv) = exprs.get(2).and_then(|e| extract_scalar(e)) {
            if let ScalarValue::Int64(Some(v)) = sv {
                limit = *v as usize;
            }
        }

        let events = fs_udf::block_on(
            self.meta.query_fs_events(
                &self.workspace_id,
                since_id,
                path_prefix.as_deref(),
                limit,
            ),
        )
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;

        let schema = fs_events_schema();
        let n = events.len();

        let mut id_b = Int64Builder::with_capacity(n);
        let mut type_b = StringBuilder::with_capacity(n, n * 8);
        let mut path_b = StringBuilder::with_capacity(n, n * 32);
        let mut fid_b = StringBuilder::with_capacity(n, n * 36);
        let mut created_b = StringBuilder::with_capacity(n, n * 32);

        for e in &events {
            id_b.append_value(e.id);
            type_b.append_value(format!("{:?}", e.event_type).to_lowercase());
            path_b.append_value(&e.path);
            match &e.file_id {
                Some(fid) => fid_b.append_value(fid),
                None => fid_b.append_null(),
            }
            created_b.append_value(e.created_at.to_rfc3339());
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(id_b.finish()),
                Arc::new(type_b.finish()),
                Arc::new(path_b.finish()),
                Arc::new(fid_b.finish()),
                Arc::new(created_b.finish()),
            ],
        )?;

        let table = if n > 0 {
            MemTable::try_new(schema, vec![vec![batch]])?
        } else {
            MemTable::try_new(schema, vec![vec![]])?
        };

        Ok(Arc::new(table))
    }
}

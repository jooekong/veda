use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{BooleanBuilder, Int32Builder, Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::common::ScalarValue;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::Result;
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::Expr;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::{
    project_schema, DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use veda_core::store::MetadataStore;
use veda_types::{Dentry, FileRecord};

const FIRST_FILE_RECORD_COL: usize = 4;

fn files_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        // 0..3: from Dentry
        Field::new("path", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("file_id", DataType::Utf8, true),
        Field::new("is_dir", DataType::Boolean, false),
        // 4..13: from FileRecord
        Field::new("size_bytes", DataType::Int64, true),
        Field::new("mime_type", DataType::Utf8, true),
        Field::new("revision", DataType::Int32, true),
        Field::new("checksum", DataType::Utf8, true),
        Field::new("ref_count", DataType::Int32, true),
        Field::new("line_count", DataType::Int32, true),
        Field::new("storage_type", DataType::Utf8, true),
        Field::new("source_type", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, true),
        Field::new("updated_at", DataType::Utf8, true),
    ]))
}

fn storage_type_str(s: veda_types::StorageType) -> &'static str {
    match s {
        veda_types::StorageType::Inline => "inline",
        veda_types::StorageType::Chunked => "chunked",
    }
}

fn source_type_str(s: veda_types::SourceType) -> &'static str {
    match s {
        veda_types::SourceType::Text => "text",
        veda_types::SourceType::Pdf => "pdf",
        veda_types::SourceType::Image => "image",
    }
}

/// Extract the longest fixed directory prefix from a LIKE pattern on `path`.
/// e.g. "/ref/%" → Some("/ref"), "/ref/sub%" → Some("/ref"), "%foo" → None
fn extract_like_dir_prefix(pattern: &str) -> Option<String> {
    let wildcard_pos = pattern.find(|c| c == '%' || c == '_')?;
    let fixed = &pattern[..wildcard_pos];
    let last_slash = fixed.rfind('/')?;
    let dir = &fixed[..last_slash];
    if dir.is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

/// Extract a path prefix from filters to narrow the scan scope.
/// Returns the narrowest (most specific) prefix found.
fn extract_path_prefix(filters: &[Expr]) -> Option<String> {
    let mut best: Option<String> = None;
    for expr in filters {
        let prefix = match expr {
            Expr::Like(like) if !like.negated && !like.case_insensitive => {
                let is_path_col = matches!(like.expr.as_ref(),
                    Expr::Column(c) if c.name == "path");
                if !is_path_col {
                    continue;
                }
                match like.pattern.as_ref() {
                    Expr::Literal(ScalarValue::Utf8(Some(pat)), _) => extract_like_dir_prefix(pat),
                    _ => None,
                }
            }
            Expr::BinaryExpr(bin) => {
                use datafusion::logical_expr::Operator;
                if bin.op != Operator::Eq {
                    continue;
                }
                let lit_expr = match (bin.left.as_ref(), bin.right.as_ref()) {
                    (Expr::Column(c), lit) if c.name == "path" => lit,
                    (lit, Expr::Column(c)) if c.name == "path" => lit,
                    _ => continue,
                };
                match lit_expr {
                    Expr::Literal(ScalarValue::Utf8(Some(val)), _) => {
                        val.rfind('/').and_then(|pos| {
                            let dir = &val[..pos];
                            if dir.is_empty() {
                                None
                            } else {
                                Some(dir.to_string())
                            }
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(p) = prefix {
            best = Some(match best {
                Some(b) if p.len() > b.len() => p,
                Some(b) => b,
                None => p,
            });
        }
    }
    best
}

fn needs_file_records(projection: Option<&Vec<usize>>) -> bool {
    match projection {
        None => true,
        Some(cols) => cols.iter().any(|&i| i >= FIRST_FILE_RECORD_COL),
    }
}

pub struct FilesTable {
    meta: Arc<dyn MetadataStore>,
    workspace_id: String,
}

impl FilesTable {
    pub fn new(meta: Arc<dyn MetadataStore>, workspace_id: String) -> Self {
        Self { meta, workspace_id }
    }
}

impl Debug for FilesTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("FilesTable")
    }
}

#[async_trait]
impl TableProvider for FilesTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        files_schema()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| match f {
                Expr::Like(like) if !like.negated && !like.case_insensitive => {
                    let is_path = matches!(like.expr.as_ref(),
                        Expr::Column(c) if c.name == "path");
                    if is_path {
                        TableProviderFilterPushDown::Inexact
                    } else {
                        TableProviderFilterPushDown::Unsupported
                    }
                }
                Expr::BinaryExpr(bin) => {
                    use datafusion::logical_expr::Operator;
                    if bin.op != Operator::Eq {
                        return TableProviderFilterPushDown::Unsupported;
                    }
                    let is_path_eq = matches!(
                        (bin.left.as_ref(), bin.right.as_ref()),
                        (Expr::Column(c), _) | (_, Expr::Column(c))
                        if c.name == "path"
                    );
                    if is_path_eq {
                        TableProviderFilterPushDown::Inexact
                    } else {
                        TableProviderFilterPushDown::Unsupported
                    }
                }
                _ => TableProviderFilterPushDown::Unsupported,
            })
            .collect())
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        const MAX_SCAN_ENTRIES: usize = 100_000;

        let path_prefix = extract_path_prefix(filters);

        let to_df_err =
            |e: veda_types::VedaError| datafusion::error::DataFusionError::External(Box::new(e));

        let scan_root = path_prefix.as_deref().unwrap_or("/");
        let mut all: Vec<Dentry> = self
            .meta
            .list_dentries_under(&self.workspace_id, scan_root)
            .await
            .map_err(to_df_err)?;

        if all.len() > MAX_SCAN_ENTRIES {
            return Err(datafusion::error::DataFusionError::Execution(format!(
                "files table scan exceeded {MAX_SCAN_ENTRIES} entries, \
                     consider narrowing your query"
            )));
        }

        let effective_limit = if filters.is_empty() { limit } else { None };

        if let Some(lim) = effective_limit {
            all.truncate(lim);
        }

        let file_map = if needs_file_records(projection) {
            let file_ids: Vec<String> = all
                .iter()
                .filter(|d| !d.is_dir)
                .filter_map(|d| d.file_id.clone())
                .collect();
            let file_records = self
                .meta
                .get_files_batch(&file_ids)
                .await
                .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;
            file_records
                .into_iter()
                .map(|f| (f.id.clone(), f))
                .collect()
        } else {
            HashMap::new()
        };

        let schema = files_schema();
        let projected = project_schema(&schema, projection)?;
        Ok(Arc::new(FilesExec::new(
            all,
            file_map,
            schema,
            projected,
            effective_limit,
        )))
    }
}

#[derive(Clone)]
struct FilesExec {
    dentries: Vec<Dentry>,
    file_map: HashMap<String, FileRecord>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    limit: Option<usize>,
    cache: Arc<PlanProperties>,
}

impl Debug for FilesExec {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilesExec")
            .field("entries", &self.dentries.len())
            .finish()
    }
}

impl FilesExec {
    fn new(
        dentries: Vec<Dentry>,
        file_map: HashMap<String, FileRecord>,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        limit: Option<usize>,
    ) -> Self {
        let cache = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            dentries,
            file_map,
            full_schema,
            projected_schema,
            limit,
            cache: Arc::new(cache),
        }
    }
}

impl DisplayAs for FilesExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "FilesExec: {} entries", self.dentries.len())
    }
}

impl ExecutionPlan for FilesExec {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn name(&self) -> &'static str {
        "FilesExec"
    }
    fn properties(&self) -> &Arc<PlanProperties> {
        &self.cache
    }
    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }
    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let emit_count = match self.limit {
            Some(lim) => self.dentries.len().min(lim),
            None => self.dentries.len(),
        };
        let entries = &self.dentries[..emit_count];
        let n = entries.len();

        let mut path_b = StringBuilder::with_capacity(n, n * 32);
        let mut name_b = StringBuilder::with_capacity(n, n * 16);
        let mut file_id_b = StringBuilder::with_capacity(n, n * 36);
        let mut is_dir_b = BooleanBuilder::with_capacity(n);
        let mut size_b = Int64Builder::with_capacity(n);
        let mut mime_b = StringBuilder::with_capacity(n, n * 16);
        let mut rev_b = Int32Builder::with_capacity(n);
        let mut cksum_b = StringBuilder::with_capacity(n, n * 64);
        let mut ref_count_b = Int32Builder::with_capacity(n);
        let mut line_count_b = Int32Builder::with_capacity(n);
        let mut storage_type_b = StringBuilder::with_capacity(n, n * 8);
        let mut source_type_b = StringBuilder::with_capacity(n, n * 8);
        let mut created_at_b = StringBuilder::with_capacity(n, n * 32);
        let mut updated_at_b = StringBuilder::with_capacity(n, n * 32);

        for d in entries {
            path_b.append_value(&d.path);
            name_b.append_value(&d.name);
            match &d.file_id {
                Some(fid) => file_id_b.append_value(fid),
                None => file_id_b.append_null(),
            }
            is_dir_b.append_value(d.is_dir);
            let fr = d.file_id.as_ref().and_then(|fid| self.file_map.get(fid));
            match fr {
                Some(f) => {
                    size_b.append_value(f.size_bytes);
                    mime_b.append_value(&f.mime_type);
                    rev_b.append_value(f.revision);
                    cksum_b.append_value(&f.checksum_sha256);
                    ref_count_b.append_value(f.ref_count);
                    match f.line_count {
                        Some(lc) => line_count_b.append_value(lc),
                        None => line_count_b.append_null(),
                    }
                    storage_type_b.append_value(storage_type_str(f.storage_type));
                    source_type_b.append_value(source_type_str(f.source_type));
                    created_at_b.append_value(f.created_at.to_rfc3339());
                    updated_at_b.append_value(f.updated_at.to_rfc3339());
                }
                None => {
                    size_b.append_null();
                    mime_b.append_null();
                    rev_b.append_null();
                    cksum_b.append_null();
                    ref_count_b.append_null();
                    line_count_b.append_null();
                    storage_type_b.append_null();
                    source_type_b.append_null();
                    created_at_b.append_null();
                    updated_at_b.append_null();
                }
            }
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(path_b.finish()),
                Arc::new(name_b.finish()),
                Arc::new(file_id_b.finish()),
                Arc::new(is_dir_b.finish()),
                Arc::new(size_b.finish()),
                Arc::new(mime_b.finish()),
                Arc::new(rev_b.finish()),
                Arc::new(cksum_b.finish()),
                Arc::new(ref_count_b.finish()),
                Arc::new(line_count_b.finish()),
                Arc::new(storage_type_b.finish()),
                Arc::new(source_type_b.finish()),
                Arc::new(created_at_b.finish()),
                Arc::new(updated_at_b.finish()),
            ],
        )?;

        let projected = if self.full_schema == self.projected_schema {
            batch
        } else {
            let indices: Vec<usize> = self
                .projected_schema
                .fields()
                .iter()
                .filter_map(|f| self.full_schema.index_of(f.name()).ok())
                .collect();
            batch.project(&indices)?
        };

        Ok(Box::pin(MemoryStream::try_new(
            vec![projected],
            self.projected_schema.clone(),
            None,
        )?))
    }
}

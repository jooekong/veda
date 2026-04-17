use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{BooleanBuilder, Int32Builder, Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use async_trait::async_trait;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::Result;
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::Expr;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream, project_schema,
};
use veda_core::store::MetadataStore;
use veda_types::{Dentry, FileRecord};

fn files_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("is_dir", DataType::Boolean, false),
        Field::new("size_bytes", DataType::Int64, true),
        Field::new("mime_type", DataType::Utf8, true),
        Field::new("revision", DataType::Int32, true),
        Field::new("checksum", DataType::Utf8, true),
    ]))
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
    fn as_any(&self) -> &dyn Any { self }

    fn schema(&self) -> SchemaRef { files_schema() }

    fn table_type(&self) -> TableType { TableType::Base }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        const MAX_SCAN_ENTRIES: usize = 100_000;
        let dentries = self.meta
            .list_dentries(&self.workspace_id, "/")
            .await
            .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;
        let mut all: Vec<Dentry> = dentries;
        let mut queue: Vec<String> = all.iter()
            .filter(|d| d.is_dir)
            .map(|d| d.path.clone())
            .collect();
        while let Some(dir) = queue.pop() {
            let children = self.meta
                .list_dentries(&self.workspace_id, &dir)
                .await
                .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;
            for c in &children {
                if c.is_dir {
                    queue.push(c.path.clone());
                }
            }
            all.extend(children);
            if all.len() > MAX_SCAN_ENTRIES {
                return Err(datafusion::error::DataFusionError::Execution(
                    format!(
                        "files table scan exceeded {MAX_SCAN_ENTRIES} entries, \
                         consider narrowing your query"
                    ),
                ));
            }
        }

        let file_ids: Vec<String> = all.iter()
            .filter(|d| !d.is_dir)
            .filter_map(|d| d.file_id.clone())
            .collect();
        let file_records = self.meta.get_files_batch(&file_ids).await
            .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;
        let file_map: HashMap<String, FileRecord> = file_records
            .into_iter()
            .map(|f| (f.id.clone(), f))
            .collect();

        let schema = files_schema();
        let projected = project_schema(&schema, projection)?;
        Ok(Arc::new(FilesExec::new(
            all,
            file_map,
            schema,
            projected,
        )))
    }
}

#[derive(Clone)]
struct FilesExec {
    dentries: Vec<Dentry>,
    file_map: HashMap<String, FileRecord>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
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
    ) -> Self {
        let cache = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self { dentries, file_map, full_schema, projected_schema, cache: Arc::new(cache) }
    }
}

impl DisplayAs for FilesExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "FilesExec: {} entries", self.dentries.len())
    }
}

impl ExecutionPlan for FilesExec {
    fn as_any(&self) -> &dyn Any { self }
    fn name(&self) -> &'static str { "FilesExec" }
    fn properties(&self) -> &Arc<PlanProperties> { &self.cache }
    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> { vec![] }
    fn with_new_children(
        self: Arc<Self>,
        _: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> { Ok(self) }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let n = self.dentries.len();
        let mut path_b = StringBuilder::with_capacity(n, n * 32);
        let mut name_b = StringBuilder::with_capacity(n, n * 16);
        let mut is_dir_b = BooleanBuilder::with_capacity(n);
        let mut size_b = Int64Builder::with_capacity(n);
        let mut mime_b = StringBuilder::with_capacity(n, n * 16);
        let mut rev_b = Int32Builder::with_capacity(n);
        let mut cksum_b = StringBuilder::with_capacity(n, n * 64);

        for d in &self.dentries {
            path_b.append_value(&d.path);
            name_b.append_value(&d.name);
            is_dir_b.append_value(d.is_dir);
            let fr = d.file_id.as_ref().and_then(|fid| self.file_map.get(fid));
            match fr {
                Some(f) => {
                    size_b.append_value(f.size_bytes);
                    mime_b.append_value(&f.mime_type);
                    rev_b.append_value(f.revision);
                    cksum_b.append_value(&f.checksum_sha256);
                }
                None => {
                    size_b.append_null();
                    mime_b.append_null();
                    rev_b.append_null();
                    cksum_b.append_null();
                }
            }
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(path_b.finish()),
                Arc::new(name_b.finish()),
                Arc::new(is_dir_b.finish()),
                Arc::new(size_b.finish()),
                Arc::new(mime_b.finish()),
                Arc::new(rev_b.finish()),
                Arc::new(cksum_b.finish()),
            ],
        )?;

        let projected = if self.full_schema == self.projected_schema {
            batch
        } else {
            let indices: Vec<usize> = self.projected_schema.fields().iter()
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

use std::any::Any;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{RecordBatch, StringBuilder};
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
use veda_core::store::CollectionVectorStore;
use veda_types::{CollectionSchema, FieldDefinition};

fn build_schema(fields: &[FieldDefinition]) -> SchemaRef {
    let mut arrow_fields = vec![Field::new("id", DataType::Utf8, false)];
    for fd in fields {
        let dt = match fd.field_type.as_str() {
            "int" | "int64" => DataType::Utf8,
            "float" | "float64" => DataType::Utf8,
            _ => DataType::Utf8,
        };
        arrow_fields.push(Field::new(&fd.name, dt, true));
    }
    Arc::new(Schema::new(arrow_fields))
}

pub struct CollectionTable {
    coll_vector: Arc<dyn CollectionVectorStore>,
    workspace_id: String,
    collection: CollectionSchema,
    milvus_name: String,
    schema: SchemaRef,
}

impl CollectionTable {
    pub fn new(
        coll_vector: Arc<dyn CollectionVectorStore>,
        workspace_id: String,
        collection: CollectionSchema,
    ) -> Self {
        let fields: Vec<FieldDefinition> = serde_json::from_value(
            collection.schema_json.clone(),
        ).unwrap_or_default();
        let schema = build_schema(&fields);
        let milvus_name = format!("veda_coll_{}", collection.id.replace('-', "_"));
        Self { coll_vector, workspace_id, collection, milvus_name, schema }
    }
}

impl Debug for CollectionTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "CollectionTable({})", self.collection.name)
    }
}

#[async_trait]
impl TableProvider for CollectionTable {
    fn as_any(&self) -> &dyn Any { self }

    fn schema(&self) -> SchemaRef { self.schema.clone() }

    fn table_type(&self) -> TableType { TableType::Base }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let dim = self.collection.embedding_dim.unwrap_or(1536) as usize;
        let zero_vec = vec![0.0f32; dim];

        let rows = self.coll_vector
            .search_collection(
                &self.milvus_name,
                &self.workspace_id,
                &zero_vec,
                16384,
            )
            .await
            .map_err(|e| datafusion::error::DataFusionError::External(Box::new(e)))?;

        let projected = project_schema(&self.schema, projection)?;
        Ok(Arc::new(CollectionExec::new(
            rows,
            self.schema.clone(),
            projected,
        )))
    }
}

#[derive(Debug, Clone)]
struct CollectionExec {
    rows: Vec<serde_json::Value>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    cache: Arc<PlanProperties>,
}

impl CollectionExec {
    fn new(
        rows: Vec<serde_json::Value>,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
    ) -> Self {
        let cache = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self { rows, full_schema, projected_schema, cache: Arc::new(cache) }
    }
}

impl DisplayAs for CollectionExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "CollectionExec: {} rows", self.rows.len())
    }
}

impl ExecutionPlan for CollectionExec {
    fn as_any(&self) -> &dyn Any { self }
    fn name(&self) -> &'static str { "CollectionExec" }
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
        let n = self.rows.len();
        let field_count = self.full_schema.fields().len();
        let mut builders: Vec<StringBuilder> = (0..field_count)
            .map(|_| StringBuilder::with_capacity(n, n * 32))
            .collect();

        for row in &self.rows {
            let obj = row.as_object();
            for (i, field) in self.full_schema.fields().iter().enumerate() {
                let val = obj
                    .and_then(|o| o.get(field.name()))
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                match val {
                    Some(s) => builders[i].append_value(&s),
                    None => builders[i].append_null(),
                }
            }
        }

        let arrays: Vec<Arc<dyn arrow::array::Array>> = builders
            .iter_mut()
            .map(|b| Arc::new(b.finish()) as Arc<dyn arrow::array::Array>)
            .collect();

        let batch = RecordBatch::try_new(self.full_schema.clone(), arrays)?;

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

use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow_csv::ReaderBuilder;
use datafusion::common::Result;

pub fn parse_csv(
    content: &str,
    delimiter: u8,
    has_header: bool,
    path: &str,
) -> Result<RecordBatch> {
    let cursor = std::io::Cursor::new(content.as_bytes());

    let (inferred_schema, _) = arrow_csv::reader::Format::default()
        .with_delimiter(delimiter)
        .with_header(has_header)
        .infer_schema(cursor, Some(100))?;

    let out_schema = build_output_schema(&inferred_schema, path);

    if content.trim().is_empty() {
        return Ok(RecordBatch::new_empty(out_schema));
    }

    let inferred_ref: SchemaRef = Arc::new(inferred_schema.clone());
    let cursor2 = std::io::Cursor::new(content.as_bytes());
    let reader = ReaderBuilder::new(inferred_ref)
        .with_delimiter(delimiter)
        .with_header(has_header)
        .build(cursor2)?;

    let mut total_rows = 0usize;
    let mut batches: Vec<RecordBatch> = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        total_rows += batch.num_rows();
        batches.push(batch);
    }

    if batches.is_empty() {
        return Ok(RecordBatch::new_empty(out_schema));
    }

    let combined = arrow::compute::concat_batches(&Arc::new(inferred_schema), &batches)?;

    let mut ln_b = Int64Builder::with_capacity(total_rows);
    for i in 0..total_rows {
        ln_b.append_value((i + 1) as i64);
    }

    let mut path_b = StringBuilder::with_capacity(total_rows, total_rows * path.len());
    for _ in 0..total_rows {
        path_b.append_value(path);
    }

    let mut columns: Vec<arrow::array::ArrayRef> = Vec::new();
    columns.push(Arc::new(ln_b.finish()));
    for i in 0..combined.num_columns() {
        columns.push(combined.column(i).clone());
    }
    columns.push(Arc::new(path_b.finish()));

    Ok(RecordBatch::try_new(out_schema, columns)?)
}

fn build_output_schema(inferred: &Schema, _path: &str) -> SchemaRef {
    let mut fields: Vec<Field> = vec![Field::new("_line_number", DataType::Int64, false)];
    for f in inferred.fields() {
        fields.push(f.as_ref().clone());
    }
    fields.push(Field::new("_path", DataType::Utf8, false));
    Arc::new(Schema::new(fields))
}

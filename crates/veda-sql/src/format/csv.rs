use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow_csv::ReaderBuilder;
use datafusion::common::Result;

pub fn parse_csv(content: &str, delimiter: u8, has_header: bool, path: &str) -> Result<RecordBatch> {
    if content.trim().is_empty() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("_line_number", DataType::Int64, false),
            Field::new("line", DataType::Utf8, false),
            Field::new("_path", DataType::Utf8, false),
        ]));
        return Ok(RecordBatch::new_empty(schema));
    }

    let cursor = std::io::Cursor::new(content.as_bytes());

    let (inferred_schema, _) = arrow_csv::reader::Format::default()
        .with_delimiter(delimiter)
        .with_header(has_header)
        .infer_schema(cursor, Some(100))?;

    let inferred_ref: SchemaRef = Arc::new(inferred_schema.clone());

    let cursor2 = std::io::Cursor::new(content.as_bytes());
    let reader = ReaderBuilder::new(inferred_ref)
        .with_delimiter(delimiter)
        .with_header(has_header)
        .build(cursor2)?;

    let mut all_columns: Vec<arrow::array::ArrayRef> = Vec::new();
    let mut total_rows = 0usize;
    let mut batches: Vec<RecordBatch> = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        total_rows += batch.num_rows();
        batches.push(batch);
    }

    if batches.is_empty() {
        let mut fields: Vec<Field> = vec![Field::new("_line_number", DataType::Int64, false)];
        for f in inferred_schema.fields() {
            fields.push(f.as_ref().clone());
        }
        fields.push(Field::new("_path", DataType::Utf8, false));
        let schema = Arc::new(Schema::new(fields));
        return Ok(RecordBatch::new_empty(schema));
    }

    let combined = arrow::compute::concat_batches(&Arc::new(inferred_schema.clone()), &batches)?;

    let mut ln_b = Int64Builder::with_capacity(total_rows);
    for i in 0..total_rows {
        ln_b.append_value((i + 1) as i64);
    }

    let mut path_b = StringBuilder::with_capacity(total_rows, total_rows * path.len());
    for _ in 0..total_rows {
        path_b.append_value(path);
    }

    let mut fields: Vec<Field> = vec![Field::new("_line_number", DataType::Int64, false)];
    for f in inferred_schema.fields() {
        fields.push(f.as_ref().clone());
    }
    fields.push(Field::new("_path", DataType::Utf8, false));
    let out_schema = Arc::new(Schema::new(fields));

    all_columns.push(Arc::new(ln_b.finish()));
    for i in 0..combined.num_columns() {
        all_columns.push(combined.column(i).clone());
    }
    all_columns.push(Arc::new(path_b.finish()));

    Ok(RecordBatch::try_new(out_schema, all_columns)?)
}

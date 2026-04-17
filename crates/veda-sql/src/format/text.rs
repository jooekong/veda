use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::Result;

pub fn text_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("_line_number", DataType::Int64, false),
        Field::new("line", DataType::Utf8, false),
        Field::new("_path", DataType::Utf8, false),
    ]))
}

pub fn parse_text(content: &str, path: &str) -> Result<RecordBatch> {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();

    let mut ln_b = Int64Builder::with_capacity(n);
    let mut line_b = StringBuilder::with_capacity(n, content.len());
    let mut path_b = StringBuilder::with_capacity(n, n * path.len());

    for (i, line) in lines.iter().enumerate() {
        ln_b.append_value((i + 1) as i64);
        line_b.append_value(line);
        path_b.append_value(path);
    }

    Ok(RecordBatch::try_new(
        text_schema(),
        vec![
            Arc::new(ln_b.finish()),
            Arc::new(line_b.finish()),
            Arc::new(path_b.finish()),
        ],
    )?)
}

use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use datafusion::common::Result;

use super::line_schema;

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
        line_schema(),
        vec![
            Arc::new(ln_b.finish()),
            Arc::new(line_b.finish()),
            Arc::new(path_b.finish()),
        ],
    )?)
}

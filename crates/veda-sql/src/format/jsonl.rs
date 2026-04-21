use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use datafusion::common::Result;
use tracing::warn;

use super::line_schema;

pub fn parse_jsonl(content: &str, path: &str) -> Result<RecordBatch> {
    let mut ln_b = Int64Builder::new();
    let mut line_b = StringBuilder::new();
    let mut path_b = StringBuilder::new();

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
            warn!(
                path = path,
                line_number = i + 1,
                "skipping invalid JSON line"
            );
            continue;
        }
        ln_b.append_value((i + 1) as i64);
        line_b.append_value(trimmed);
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

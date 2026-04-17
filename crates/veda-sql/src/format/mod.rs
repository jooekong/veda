mod csv;
mod jsonl;
mod text;

use std::sync::Arc;
use arrow::array::RecordBatch;
use datafusion::common::Result;

#[derive(Debug, Clone, PartialEq)]
pub enum FileFormat {
    Csv { delimiter: u8, has_header: bool },
    Tsv,
    JsonLines,
    PlainText,
}

pub fn detect_format(path: &str) -> FileFormat {
    match path.rsplit('.').next().map(|e| e.to_ascii_lowercase()) {
        Some(ref ext) if ext == "csv" => FileFormat::Csv { delimiter: b',', has_header: true },
        Some(ref ext) if ext == "tsv" => FileFormat::Tsv,
        Some(ref ext) if ext == "jsonl" || ext == "ndjson" => FileFormat::JsonLines,
        _ => FileFormat::PlainText,
    }
}

pub fn parse_file(content: &str, path: &str, format: &FileFormat) -> Result<RecordBatch> {
    match format {
        FileFormat::Csv { delimiter, has_header } => csv::parse_csv(content, *delimiter, *has_header, path),
        FileFormat::Tsv => csv::parse_csv(content, b'\t', true, path),
        FileFormat::JsonLines => jsonl::parse_jsonl(content, path),
        FileFormat::PlainText => text::parse_text(content, path),
    }
}

/// Build a schema for directory listing mode.
pub fn dir_listing_schema() -> Arc<arrow::datatypes::Schema> {
    use arrow::datatypes::{DataType, Field, Schema};
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("size_bytes", DataType::Int64, true),
        Field::new("mtime", DataType::Utf8, true),
    ]))
}

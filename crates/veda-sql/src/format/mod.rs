mod csv;
mod jsonl;
mod text;

use std::sync::Arc;
use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::common::Result;

pub const MAX_SINGLE_FILE_BYTES: usize = 50 * 1024 * 1024; // 50MB

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

/// Schema shared by plain text and JSONL formats: (_line_number, line, _path).
pub fn line_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("_line_number", DataType::Int64, false),
        Field::new("line", DataType::Utf8, false),
        Field::new("_path", DataType::Utf8, false),
    ]))
}

/// Build an empty RecordBatch matching the format detected from a path.
pub fn empty_batch_for_format(path: &str) -> Result<RecordBatch> {
    let fmt = detect_format(path);
    parse_file("", path, &fmt)
}

/// Schema for directory listing mode.
pub fn dir_listing_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("size_bytes", DataType::Int64, true),
        Field::new("mtime", DataType::Utf8, true),
    ]))
}

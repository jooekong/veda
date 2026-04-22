use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{Int64Builder, RecordBatch, StringBuilder};
use datafusion::catalog::{TableFunctionImpl, TableProvider};
use datafusion::common::{plan_err, ScalarValue};
use datafusion::datasource::memory::MemTable;
use datafusion::error::Result;
use datafusion::logical_expr::Expr;

use veda_core::service::fs::FsService;
use veda_types::Dentry;

use crate::format::{self, FileFormat};
use crate::fs_udf;

const MAX_GLOB_TOTAL_READ: usize = 100 * 1024 * 1024; // 100MB
const MAX_GLOB_FILE_COUNT: usize = 10_000;

/// Factory that implements DataFusion's `TableFunctionImpl` for `veda_fs(path)`.
pub struct VedaFsTableFactory {
    pub workspace_id: String,
    pub fs_service: Arc<FsService>,
}

impl Debug for VedaFsTableFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VedaFsTableFactory")
            .field("workspace_id", &self.workspace_id)
            .finish()
    }
}

impl TableFunctionImpl for VedaFsTableFactory {
    fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let path = match exprs.first() {
            Some(Expr::Literal(ScalarValue::Utf8(Some(s)), _)) => s.clone(),
            _ => return plan_err!("veda_fs() requires a string path argument"),
        };

        let ws = self.workspace_id.clone();
        let fs = self.fs_service.clone();

        let mode = detect_mode(&path);

        match mode {
            FsMode::DirListing => {
                let dentries =
                    fs_udf::block_on(fs.list_dir_recursive(&ws, &path, MAX_GLOB_FILE_COUNT))
                        .map_err(|e| {
                            datafusion::error::DataFusionError::Execution(e.to_string())
                        })?;

                let batch = build_dir_listing_batch(&dentries, &fs, &ws)?;
                let schema = format::dir_listing_schema();
                let table = MemTable::try_new(schema, vec![vec![batch]])?;
                Ok(Arc::new(table))
            }
            FsMode::FileRead => {
                let detected = format::detect_format(&path);
                let batch = read_single_file(&fs, &ws, &path, &detected)?;
                let schema = batch.schema();
                let table = MemTable::try_new(schema, vec![vec![batch]])?;
                Ok(Arc::new(table))
            }
            FsMode::Glob => {
                let matching = fs_udf::block_on(fs.glob_files(&ws, &path, MAX_GLOB_FILE_COUNT))
                    .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;

                let batches = read_glob_files(&fs, &ws, &matching)?;

                if batches.is_empty() {
                    let empty = format::empty_batch_for_format(&path)?;
                    let table = MemTable::try_new(empty.schema(), vec![vec![]])?;
                    return Ok(Arc::new(table));
                }

                let schema = batches[0].schema();
                let table = MemTable::try_new(schema, vec![batches])?;
                Ok(Arc::new(table))
            }
        }
    }
}

#[derive(Debug)]
enum FsMode {
    DirListing,
    FileRead,
    Glob,
}

fn detect_mode(path: &str) -> FsMode {
    if path.contains('*') || path.contains('?') {
        FsMode::Glob
    } else if path.ends_with('/') {
        FsMode::DirListing
    } else {
        FsMode::FileRead
    }
}

fn build_dir_listing_batch(dentries: &[Dentry], fs: &FsService, _ws: &str) -> Result<RecordBatch> {
    let n = dentries.len();

    let file_ids: Vec<&str> = dentries
        .iter()
        .filter_map(|d| d.file_id.as_deref())
        .collect();
    let mut file_map: HashMap<String, veda_types::FileRecord> = HashMap::new();
    for fid in &file_ids {
        if let Ok(Some(fr)) = fs_udf::block_on(fs.get_file(fid)) {
            file_map.insert(fid.to_string(), fr);
        }
    }

    let mut path_b = StringBuilder::with_capacity(n, n * 32);
    let mut name_b = StringBuilder::with_capacity(n, n * 16);
    let mut type_b = StringBuilder::with_capacity(n, n * 8);
    let mut size_b = Int64Builder::with_capacity(n);
    let mut mtime_b = StringBuilder::with_capacity(n, n * 32);

    for d in dentries {
        path_b.append_value(&d.path);
        name_b.append_value(&d.name);
        type_b.append_value(if d.is_dir { "directory" } else { "file" });

        match d.file_id.as_deref().and_then(|fid| file_map.get(fid)) {
            Some(fr) => size_b.append_value(fr.size_bytes),
            None => size_b.append_null(),
        }
        mtime_b.append_value(d.updated_at.to_rfc3339());
    }

    let schema = format::dir_listing_schema();
    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(path_b.finish()),
            Arc::new(name_b.finish()),
            Arc::new(type_b.finish()),
            Arc::new(size_b.finish()),
            Arc::new(mtime_b.finish()),
        ],
    )?)
}

fn read_single_file(fs: &FsService, ws: &str, path: &str, fmt: &FileFormat) -> Result<RecordBatch> {
    let content = fs_udf::bounded_read_file(fs, ws, path)
        .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;
    format::parse_file(&content, path, fmt)
}

fn read_glob_files(fs: &FsService, ws: &str, dentries: &[Dentry]) -> Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    let mut total_read: usize = 0;
    let mut first_format: Option<FileFormat> = None;

    for d in dentries {
        let fmt = format::detect_format(&d.path);
        if let Some(ref ff) = first_format {
            if std::mem::discriminant(ff) != std::mem::discriminant(&fmt) {
                return Err(datafusion::error::DataFusionError::Execution(
                    format!("glob matches files of mixed formats ({:?} vs {:?}), use a more specific pattern", ff, fmt),
                ));
            }
        } else {
            first_format = Some(fmt.clone());
        }

        let content = fs_udf::block_on(fs.read_file(ws, &d.path))
            .map_err(|e| datafusion::error::DataFusionError::Execution(e.to_string()))?;

        total_read += content.len();
        if total_read > MAX_GLOB_TOTAL_READ {
            return Err(datafusion::error::DataFusionError::Execution(format!(
                "glob read budget exceeded: {} bytes > {}MB limit",
                total_read,
                MAX_GLOB_TOTAL_READ / 1024 / 1024
            )));
        }

        let batch = format::parse_file(&content, &d.path, &fmt)?;
        if batch.num_rows() > 0 {
            batches.push(batch);
        }
    }

    Ok(batches)
}

use std::sync::Arc;

use arrow::array::{Array, BooleanBuilder, Int64Builder, StringBuilder, StringArray};
use arrow::datatypes::DataType;
use datafusion::common::Result;
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, TypeSignature,
    Volatility,
};
use tracing::info;
use veda_core::service::fs::FsService;
use veda_types::VedaError;

const MAX_READ_SIZE: i64 = 50 * 1024 * 1024; // 50 MB

pub struct FsUdfContext {
    pub workspace_id: String,
    pub fs_service: Arc<FsService>,
    pub read_only: bool,
}

impl std::fmt::Debug for FsUdfContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsUdfContext")
            .field("workspace_id", &self.workspace_id)
            .finish()
    }
}

impl std::hash::Hash for FsUdfContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.workspace_id.hash(state);
    }
}

impl PartialEq for FsUdfContext {
    fn eq(&self, other: &Self) -> bool {
        self.workspace_id == other.workspace_id
    }
}

impl Eq for FsUdfContext {}

pub fn register_all(ctx: &datafusion::prelude::SessionContext, fs_ctx: Arc<FsUdfContext>) {
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_read", 1, DataType::Utf8, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_write", 2, DataType::Int64, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_append", 2, DataType::Int64, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_exists", 1, DataType::Boolean, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_size", 1, DataType::Int64, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_mtime", 1, DataType::Utf8, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_remove", 1, DataType::Int64, fs_ctx.clone())));
    ctx.register_udf(ScalarUDF::from(FsScalarUdf::new("veda_mkdir", 1, DataType::Boolean, fs_ctx)));
}

pub(crate) fn block_on<F: std::future::Future<Output: Send> + Send>(f: F) -> F::Output {
    let handle = tokio::runtime::Handle::current();
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(f))
        }
        _ => {
            std::thread::scope(|s| {
                s.spawn(|| handle.block_on(f)).join().unwrap()
            })
        }
    }
}

fn get_string_values(args: &[ColumnarValue], idx: usize, num_rows: usize) -> Result<Vec<String>> {
    match &args[idx] {
        ColumnarValue::Scalar(scalar) => {
            let arr = scalar.to_array()?;
            let s = arr
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|a| a.value(0).to_string())
                .unwrap_or_default();
            Ok(vec![s; num_rows])
        }
        ColumnarValue::Array(arr) => {
            let str_arr = arr
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| datafusion::error::DataFusionError::Execution(
                    "expected string array argument".to_string(),
                ))?;
            Ok((0..str_arr.len()).map(|i| str_arr.value(i).to_string()).collect())
        }
    }
}

fn not_found_to_none(e: &VedaError) -> bool {
    matches!(e, VedaError::NotFound(_))
}

fn check_write_allowed(ctx: &FsUdfContext) -> Result<()> {
    if ctx.read_only {
        return Err(datafusion::error::DataFusionError::Execution(
            "permission denied: write UDF not allowed with read-only key".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct FsScalarUdf {
    udf_name: String,
    sig: Signature,
    ret: DataType,
    ctx: Arc<FsUdfContext>,
}

impl FsScalarUdf {
    fn new(name: &str, arity: usize, ret: DataType, ctx: Arc<FsUdfContext>) -> Self {
        let sig = Signature::new(
            TypeSignature::Exact(vec![DataType::Utf8; arity]),
            Volatility::Volatile,
        );
        Self {
            udf_name: name.to_string(),
            sig,
            ret,
            ctx,
        }
    }
}

impl ScalarUDFImpl for FsScalarUdf {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &str { &self.udf_name }
    fn signature(&self) -> &Signature { &self.sig }
    fn return_type(&self, _: &[DataType]) -> Result<DataType> { Ok(self.ret.clone()) }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let num_rows = args.number_rows;
        let ws = &self.ctx.workspace_id;
        let fs = &self.ctx.fs_service;

        match self.udf_name.as_str() {
            "veda_read" => {
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = StringBuilder::new();
                for path in &paths {
                    let stat = block_on(fs.stat(ws, path));
                    match stat {
                        Ok(info) => {
                            if let Some(sz) = info.size_bytes {
                                if sz > MAX_READ_SIZE {
                                    return Err(exec_err(VedaError::QuotaExceeded(
                                        format!("file {} is {} bytes, exceeds {}MB limit", path, sz, MAX_READ_SIZE / 1024 / 1024),
                                    )));
                                }
                            }
                        }
                        Err(ref e) if not_found_to_none(e) => { builder.append_null(); continue; }
                        Err(e) => return Err(exec_err(e)),
                    }
                    match block_on(fs.read_file(ws, path)) {
                        Ok(content) => builder.append_value(&content),
                        Err(ref e) if not_found_to_none(e) => builder.append_null(),
                        Err(e) => return Err(exec_err(e)),
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_write" => {
                check_write_allowed(&self.ctx)?;
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let contents = get_string_values(&args.args, 1, num_rows)?;
                let mut builder = Int64Builder::with_capacity(num_rows);
                for (path, content) in paths.iter().zip(contents.iter()) {
                    info!(udf = "veda_write", workspace_id = ws, path = path, bytes = content.len());
                    block_on(fs.write_file(ws, path, content)).map_err(exec_err)?;
                    builder.append_value(content.len() as i64);
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_append" => {
                check_write_allowed(&self.ctx)?;
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let contents = get_string_values(&args.args, 1, num_rows)?;
                let mut builder = Int64Builder::with_capacity(num_rows);
                for (path, content) in paths.iter().zip(contents.iter()) {
                    info!(udf = "veda_append", workspace_id = ws, path = path, bytes = content.len());
                    block_on(fs.append_file(ws, path, content)).map_err(exec_err)?;
                    builder.append_value(content.len() as i64);
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_exists" => {
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = BooleanBuilder::with_capacity(num_rows);
                for path in &paths {
                    match block_on(fs.stat(ws, path)) {
                        Ok(_) => builder.append_value(true),
                        Err(ref e) if not_found_to_none(e) => builder.append_value(false),
                        Err(e) => return Err(exec_err(e)),
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_size" => {
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = Int64Builder::with_capacity(num_rows);
                for path in &paths {
                    match block_on(fs.stat(ws, path)) {
                        Ok(info) => match info.size_bytes {
                            Some(sz) => builder.append_value(sz),
                            None => builder.append_null(),
                        },
                        Err(ref e) if not_found_to_none(e) => builder.append_null(),
                        Err(e) => return Err(exec_err(e)),
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_mtime" => {
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = StringBuilder::new();
                for path in &paths {
                    match block_on(fs.stat(ws, path)) {
                        Ok(info) => builder.append_value(info.updated_at.to_rfc3339()),
                        Err(ref e) if not_found_to_none(e) => builder.append_null(),
                        Err(e) => return Err(exec_err(e)),
                    }
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_remove" => {
                check_write_allowed(&self.ctx)?;
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = Int64Builder::with_capacity(num_rows);
                for path in &paths {
                    info!(udf = "veda_remove", workspace_id = ws, path = path);
                    let count = block_on(fs.delete(ws, path)).map_err(exec_err)?;
                    builder.append_value(count as i64);
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            "veda_mkdir" => {
                check_write_allowed(&self.ctx)?;
                let paths = get_string_values(&args.args, 0, num_rows)?;
                let mut builder = BooleanBuilder::with_capacity(num_rows);
                for path in &paths {
                    info!(udf = "veda_mkdir", workspace_id = ws, path = path);
                    block_on(fs.mkdir(ws, path)).map_err(exec_err)?;
                    builder.append_value(true);
                }
                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            }
            _ => Err(datafusion::error::DataFusionError::Execution(
                format!("unknown fs udf: {}", self.udf_name),
            )),
        }
    }
}

fn exec_err(e: VedaError) -> datafusion::error::DataFusionError {
    datafusion::error::DataFusionError::Execution(e.to_string())
}

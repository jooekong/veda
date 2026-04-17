use std::sync::Arc;

use arrow::array::{Array, StringBuilder, StringArray};
use arrow::datatypes::DataType;
use datafusion::common::Result;
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDFImpl, Signature, TypeSignature, Volatility,
};
use veda_core::store::EmbeddingService;

use crate::fs_udf::block_on;

#[derive(Clone)]
pub struct EmbeddingUdf {
    sig: Signature,
    embedding: Arc<dyn EmbeddingService>,
}

impl std::fmt::Debug for EmbeddingUdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingUdf")
            .field("dim", &self.embedding.dimension())
            .finish()
    }
}

impl std::hash::Hash for EmbeddingUdf {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.sig.hash(state);
        self.embedding.dimension().hash(state);
    }
}

impl PartialEq for EmbeddingUdf {
    fn eq(&self, other: &Self) -> bool {
        self.embedding.dimension() == other.embedding.dimension()
    }
}

impl Eq for EmbeddingUdf {}

impl EmbeddingUdf {
    pub fn new(embedding: Arc<dyn EmbeddingService>) -> Self {
        let sig = Signature::new(
            TypeSignature::Exact(vec![DataType::Utf8]),
            Volatility::Volatile,
        );
        Self { sig, embedding }
    }
}

impl ScalarUDFImpl for EmbeddingUdf {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn name(&self) -> &str { "embedding" }
    fn signature(&self) -> &Signature { &self.sig }
    fn return_type(&self, _: &[DataType]) -> Result<DataType> { Ok(DataType::Utf8) }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let num_rows = args.number_rows;

        let (texts, is_null): (Vec<String>, Vec<bool>) = match &args.args[0] {
            ColumnarValue::Scalar(scalar) => {
                if scalar.is_null() {
                    (vec![String::new(); num_rows], vec![true; num_rows])
                } else {
                    let arr = scalar.to_array()?;
                    let s = arr
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .map(|a| a.value(0).to_string())
                        .unwrap_or_default();
                    (vec![s; num_rows], vec![false; num_rows])
                }
            }
            ColumnarValue::Array(arr) => {
                let str_arr = arr
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| datafusion::error::DataFusionError::Execution(
                        "embedding(): expected string argument".to_string(),
                    ))?;
                let texts: Vec<String> = (0..str_arr.len())
                    .map(|i| if str_arr.is_null(i) { String::new() } else { str_arr.value(i).to_string() })
                    .collect();
                let nulls: Vec<bool> = (0..str_arr.len()).map(|i| str_arr.is_null(i)).collect();
                (texts, nulls)
            }
        };

        let non_null_texts: Vec<String> = texts.iter().zip(is_null.iter())
            .filter(|(_, &n)| !n)
            .map(|(t, _)| t.clone())
            .collect();

        let vectors = if non_null_texts.is_empty() {
            Vec::new()
        } else {
            block_on(self.embedding.embed(&non_null_texts))
                .map_err(|e| datafusion::error::DataFusionError::Execution(
                    format!("embedding() failed: {e}"),
                ))?
        };

        let mut vec_iter = vectors.iter();
        let mut builder = StringBuilder::with_capacity(num_rows, num_rows * 128);
        for &null in &is_null {
            if null {
                builder.append_null();
            } else {
                let vec = vec_iter.next().ok_or_else(|| {
                    datafusion::error::DataFusionError::Execution("embedding vector count mismatch".into())
                })?;
                let json = serde_json::to_string(vec).map_err(|e| {
                    datafusion::error::DataFusionError::Execution(format!("serialize embedding: {e}"))
                })?;
                builder.append_value(&json);
            }
        }

        Ok(ColumnarValue::Array(Arc::new(builder.finish())))
    }
}

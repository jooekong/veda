use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use arrow::array::{Float64Builder, Int64Builder, RecordBatch, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::catalog::{TableFunctionImpl, TableProvider};
use datafusion::common::ScalarValue;
use datafusion::datasource::memory::MemTable;
use datafusion::error::Result;
use datafusion::logical_expr::Expr;
use tracing::warn;

use veda_core::store::{EmbeddingService, MetadataStore, VectorStore};
use veda_types::*;

use crate::fs_udf;

fn search_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("file_id", DataType::Utf8, false),
        Field::new("chunk_index", DataType::Int64, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("score", DataType::Float64, false),
        Field::new("path", DataType::Utf8, true),
    ]))
}

pub struct VedaSearchFactory {
    pub workspace_id: String,
    pub meta: Arc<dyn MetadataStore>,
    pub vector: Arc<dyn VectorStore>,
    pub embedding: Arc<dyn EmbeddingService>,
}

impl Debug for VedaSearchFactory {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VedaSearchFactory").finish()
    }
}

impl TableFunctionImpl for VedaSearchFactory {
    fn call(&self, exprs: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let query = match exprs.first() {
            Some(Expr::Literal(ScalarValue::Utf8(Some(s)), _)) => s.clone(),
            _ => {
                return Err(datafusion::error::DataFusionError::Plan(
                    "search() requires a string query as first argument".to_string(),
                ))
            }
        };

        let mode = match exprs.get(1) {
            Some(Expr::Literal(ScalarValue::Utf8(Some(s)), _)) => match s.as_str() {
                "hybrid" => SearchMode::Hybrid,
                "semantic" => SearchMode::Semantic,
                "fulltext" => SearchMode::Fulltext,
                other => {
                    return Err(datafusion::error::DataFusionError::Plan(format!(
                        "search(): unknown mode '{}', expected hybrid/semantic/fulltext",
                        other
                    )))
                }
            },
            Some(Expr::Literal(ScalarValue::Null, _)) | None => SearchMode::Hybrid,
            _ => {
                return Err(datafusion::error::DataFusionError::Plan(
                    "search(): mode (arg 2) must be a string".to_string(),
                ))
            }
        };

        let limit: usize = match exprs.get(2) {
            Some(Expr::Literal(ScalarValue::Int64(Some(v)), _)) => {
                if *v <= 0 {
                    return Err(datafusion::error::DataFusionError::Plan(format!(
                        "search(): limit must be positive, got {}",
                        v
                    )));
                }
                *v as usize
            }
            Some(Expr::Literal(ScalarValue::Null, _)) | None => 10,
            _ => {
                return Err(datafusion::error::DataFusionError::Plan(
                    "search(): limit (arg 3) must be an integer".to_string(),
                ))
            }
        };

        let hits = fs_udf::block_on(self.do_search(&query, mode, limit)).map_err(|e| {
            datafusion::error::DataFusionError::Execution(format!("search() failed: {e}"))
        })?;

        let schema = search_schema();
        let n = hits.len();

        let mut fid_b = StringBuilder::with_capacity(n, n * 36);
        let mut idx_b = Int64Builder::with_capacity(n);
        let mut content_b = StringBuilder::with_capacity(n, n * 256);
        let mut score_b = Float64Builder::with_capacity(n);
        let mut path_b = StringBuilder::with_capacity(n, n * 64);

        for h in &hits {
            fid_b.append_value(&h.file_id);
            idx_b.append_value(h.chunk_index as i64);
            content_b.append_value(&h.content);
            score_b.append_value(h.score as f64);
            match &h.path {
                Some(p) => path_b.append_value(p),
                None => path_b.append_null(),
            }
        }

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(fid_b.finish()),
                Arc::new(idx_b.finish()),
                Arc::new(content_b.finish()),
                Arc::new(score_b.finish()),
                Arc::new(path_b.finish()),
            ],
        )?;

        let table = if n > 0 {
            MemTable::try_new(schema, vec![vec![batch]])?
        } else {
            MemTable::try_new(schema, vec![vec![]])?
        };

        Ok(Arc::new(table))
    }
}

impl VedaSearchFactory {
    async fn do_search(
        &self,
        query: &str,
        mode: SearchMode,
        limit: usize,
    ) -> veda_types::Result<Vec<SearchHit>> {
        let ws = &self.workspace_id;

        let query_vector = match mode {
            SearchMode::Semantic | SearchMode::Hybrid => {
                let vectors = self.embedding.embed(&[query.to_string()]).await?;
                Some(vectors.into_iter().next().ok_or_else(|| {
                    VedaError::EmbeddingFailed("empty embedding result".to_string())
                })?)
            }
            SearchMode::Fulltext => None,
        };
        let req = SearchRequest {
            workspace_id: ws.to_string(),
            query: query.to_string(),
            mode,
            limit,
            path_prefix: None,
            query_vector,
        };
        let mut hits = self.vector.search(&req).await?;

        let missing_fids: Vec<String> = hits
            .iter()
            .filter(|h| h.path.is_none())
            .map(|h| h.file_id.clone())
            .collect();
        if !missing_fids.is_empty() {
            match self
                .meta
                .get_dentry_paths_by_file_ids(ws, &missing_fids)
                .await
            {
                Ok(path_map) => {
                    for hit in &mut hits {
                        if hit.path.is_none() {
                            hit.path = path_map.get(&hit.file_id).cloned();
                        }
                    }
                }
                Err(e) => {
                    warn!(err = %e, "search(): failed to batch-resolve paths");
                }
            }
        }

        hits.truncate(limit);
        Ok(hits)
    }
}

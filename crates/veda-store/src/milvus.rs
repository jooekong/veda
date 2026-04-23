//! Milvus REST v2 `VectorStore` implementation.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tracing::warn;
use veda_core::store::{CollectionVectorStore, VectorStore};
use veda_types::{
    ChunkWithEmbedding, FieldDefinition, Result, SearchHit, SearchMode, SearchRequest, VedaError,
};

const COLLECTION: &str = "veda_chunks";

fn storage_err(e: impl ToString) -> VedaError {
    VedaError::Storage(e.to_string())
}

/// Milvus boolean expressions use double-quoted string literals (see Milvus docs).
fn milvus_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Milvus may return `data` as a flat array of hits or as an array of per-query hit arrays.
fn flatten_entity_rows(data: Option<&Value>) -> Vec<Value> {
    let Some(data) = data else {
        return Vec::new();
    };
    match data {
        Value::Array(a) if a.iter().all(|x| x.is_object()) => a.clone(),
        Value::Array(a) => a
            .iter()
            .flat_map(|item| {
                if let Value::Array(inner) = item {
                    inner.clone()
                } else {
                    vec![item.clone()]
                }
            })
            .collect(),
        Value::Object(_) => vec![data.clone()],
        _ => Vec::new(),
    }
}

pub struct MilvusStore {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
    db_name: Option<String>,
}

impl MilvusStore {
    pub fn new(url: &str, token: Option<String>, db_name: Option<String>) -> Self {
        let base_url = url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url,
            token,
            db_name,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(tok) = &self.token {
            let v = format!("Bearer {tok}");
            h.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&v).map_err(|e| storage_err(e.to_string()))?,
            );
        }
        Ok(h)
    }

    fn inject_db(&self, body: &mut Value) {
        if let Some(db) = &self.db_name {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("dbName".to_string(), Value::String(db.clone()));
            }
        }
    }

    async fn post_v2(&self, path: &str, mut body: Value) -> Result<Value> {
        self.inject_db(&mut body);
        let resp = self
            .http
            .post(self.url(path))
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| VedaError::Storage(e.to_string()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            VedaError::Storage(format!(
                "milvus invalid json (HTTP {status}): {e}; body: {text}"
            ))
        })?;
        let code = v
            .get("code")
            .and_then(|c| c.as_i64())
            .or_else(|| v.get("code").and_then(|c| c.as_u64()).map(|u| u as i64))
            .unwrap_or(-1);
        if code != 0 {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(VedaError::Storage(format!(
                "milvus error code {code}: {msg}"
            )));
        }
        Ok(v)
    }

    async fn collection_exists(&self) -> Result<bool> {
        let v = self
            .post_v2(
                "/v2/vectordb/collections/has",
                json!({ "collectionName": COLLECTION }),
            )
            .await?;
        Ok(v["data"]["has"].as_bool().unwrap_or(false))
    }

    async fn create_collection(&self, embedding_dim: u32) -> Result<()> {
        let dim = embedding_dim as i64;
        let body = json!({
            "collectionName": COLLECTION,
            "schema": {
                "enableDynamicField": false,
                "fields": [
                    {
                        "fieldName": "id",
                        "dataType": "VarChar",
                        "isPrimary": true,
                        "elementTypeParams": { "max_length": 64 }
                    },
                    {
                        "fieldName": "workspace_id",
                        "dataType": "VarChar",
                        "elementTypeParams": { "max_length": 64 }
                    },
                    {
                        "fieldName": "file_id",
                        "dataType": "VarChar",
                        "elementTypeParams": { "max_length": 64 }
                    },
                    {
                        "fieldName": "chunk_index",
                        "dataType": "Int32"
                    },
                    {
                        "fieldName": "content",
                        "dataType": "VarChar",
                        "elementTypeParams": { "max_length": 65535 }
                    },
                    {
                        "fieldName": "vector",
                        "dataType": "FloatVector",
                        "elementTypeParams": { "dim": dim }
                    }
                ]
            }
        });
        self.post_v2("/v2/vectordb/collections/create", body)
            .await?;
        Ok(())
    }

    async fn ensure_vector_index(&self) -> Result<()> {
        let index_body = json!({
            "collectionName": COLLECTION,
            "indexParams": [{
                "index_type": "AUTOINDEX",
                "metricType": "COSINE",
                "fieldName": "vector",
                "indexName": "vector"
            }]
        });
        match self
            .post_v2("/v2/vectordb/indexes/create", index_body)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let m = e.to_string();
                if m.contains("same index name")
                    || m.contains("IndexAlreadyExists")
                    || m.contains("index already exist")
                {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn rows_to_hits(rows: &[Value], limit: usize) -> Vec<SearchHit> {
        let mut out = Vec::new();
        for row in rows.iter().take(limit) {
            let file_id = row
                .get("file_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let chunk_index = row.get("chunk_index").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            let content = row
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let score = row
                .get("distance")
                .and_then(|x| x.as_f64())
                .map(|d| d as f32)
                .or_else(|| row.get("score").and_then(|x| x.as_f64()).map(|d| d as f32))
                .unwrap_or(0.0);
            out.push(SearchHit {
                file_id,
                chunk_index,
                content,
                score,
                path: None,
            });
        }
        out
    }

    async fn ann_search(
        &self,
        workspace_id: &str,
        vector: &[f32],
        limit: usize,
        text_filter: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let ws = milvus_quote(workspace_id);
        let mut filter = format!("workspace_id == {ws}");
        if let Some(q) = text_filter {
            if !q.is_empty() {
                let pat = format!(
                    "%{}%",
                    q.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                        .replace('"', "\\\"")
                );
                filter.push_str(&format!(" && content like {}", milvus_quote(&pat)));
            }
        }
        let lim = limit.min(16_383).max(1);
        let body = json!({
            "collectionName": COLLECTION,
            "data": [vector],
            "annsField": "vector",
            "filter": filter,
            "limit": lim,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"],
            "searchParams": { "metricType": "COSINE" }
        });
        let v = self.post_v2("/v2/vectordb/entities/search", body).await?;
        let rows = flatten_entity_rows(v.get("data"));
        Ok(Self::rows_to_hits(&rows, limit))
    }

    async fn hybrid_search_remote(
        &self,
        req: &SearchRequest,
    ) -> Result<Option<Vec<SearchHit>>> {
        let qv = req.query_vector.as_ref().unwrap();
        let ws = milvus_quote(&req.workspace_id);
        let base_filter = format!("workspace_id == {ws}");
        let lim = req.limit.min(16_383).max(1);
        let search_obj = json!({
            "data": [qv],
            "annsField": "vector",
            "filter": base_filter.clone(),
            "limit": lim,
            "offset": 0,
            "ignoreGrowing": false,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"],
            "metricType": "COSINE"
        });
        let body = json!({
            "collectionName": COLLECTION,
            "search": [search_obj],
            "rerank": {
                "strategy": "rrf",
                "params": { "k": 60 }
            },
            "limit": lim,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"]
        });
        match self
            .post_v2("/v2/vectordb/entities/hybrid_search", body)
            .await
        {
            Ok(v) => {
                let rows = flatten_entity_rows(v.get("data"));
                Ok(Some(Self::rows_to_hits(&rows, req.limit)))
            }
            Err(e) => {
                warn!(err = %e, "hybrid_search_remote failed, falling back to ANN search");
                Ok(None)
            }
        }
    }

    async fn query_fulltext(
        &self,
        workspace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let ws = milvus_quote(workspace_id);
        let pat = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
                .replace('"', "\\\"")
        );
        let filter = format!(
            "workspace_id == {ws} && content like {}",
            milvus_quote(&pat)
        );
        let lim = limit.min(16_383).max(1);
        let body = json!({
            "collectionName": COLLECTION,
            "filter": filter,
            "limit": lim,
            "outputFields": ["id", "file_id", "chunk_index", "content"]
        });
        let v = self.post_v2("/v2/vectordb/entities/query", body).await?;
        let rows = flatten_entity_rows(v.get("data"));
        let mut hits = Vec::new();
        for row in rows.iter().take(limit) {
            hits.push(SearchHit {
                file_id: row
                    .get("file_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                chunk_index: row.get("chunk_index").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
                content: row
                    .get("content")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                score: 1.0,
                path: None,
            });
        }
        Ok(hits)
    }
}

#[async_trait]
impl VectorStore for MilvusStore {
    async fn upsert_chunks(&self, chunks: &[ChunkWithEmbedding]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        // 1. Upsert new chunks first (safe: at worst we have brief duplicates)
        let data: Vec<Value> = chunks
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "workspace_id": c.workspace_id,
                    "file_id": c.file_id,
                    "chunk_index": c.chunk_index,
                    "content": c.content,
                    "vector": c.vector
                })
            })
            .collect();
        let body = json!({
            "collectionName": COLLECTION,
            "data": data
        });
        self.post_v2("/v2/vectordb/entities/upsert", body).await?;

        // 2. Delete stale chunks (indices beyond the new set) — safe because new data is persisted
        let max_index = chunks.iter().map(|c| c.chunk_index).max().unwrap_or(0);
        let file_id = &chunks[0].file_id;
        let ws_id = &chunks[0].workspace_id;
        let ws = milvus_quote(ws_id);
        let fid = milvus_quote(file_id);
        let filter =
            format!("workspace_id == {ws} && file_id == {fid} && chunk_index > {max_index}");
        let del = json!({
            "collectionName": COLLECTION,
            "filter": filter
        });
        let _ = self.post_v2("/v2/vectordb/entities/delete", del).await;

        Ok(())
    }

    async fn delete_chunks(&self, workspace_id: &str, file_id: &str) -> Result<()> {
        let ws = milvus_quote(workspace_id);
        let fid = milvus_quote(file_id);
        let filter = format!("workspace_id == {ws} && file_id == {fid}");
        let body = json!({
            "collectionName": COLLECTION,
            "filter": filter
        });
        self.post_v2("/v2/vectordb/entities/delete", body).await?;
        Ok(())
    }

    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        match req.mode {
            SearchMode::Fulltext => {
                self.query_fulltext(&req.workspace_id, &req.query, req.limit)
                    .await
            }
            SearchMode::Semantic => {
                let v = req.query_vector.as_ref().ok_or_else(|| {
                    VedaError::InvalidInput("search requires query_vector for vector modes".into())
                })?;
                self.ann_search(&req.workspace_id, v, req.limit, None).await
            }
            SearchMode::Hybrid => {
                let v = req.query_vector.as_ref().ok_or_else(|| {
                    VedaError::InvalidInput("search requires query_vector for vector modes".into())
                })?;
                if v.is_empty() {
                    return Err(VedaError::InvalidInput(
                        "query_vector must be non-empty".into(),
                    ));
                }
                if let Some(hits) = self.hybrid_search_remote(req).await? {
                    return Ok(hits);
                }
                self.ann_search(&req.workspace_id, v, req.limit, Some(&req.query))
                    .await
            }
        }
    }

    async fn init_collections(&self, embedding_dim: u32) -> Result<()> {
        if !self.collection_exists().await? {
            self.create_collection(embedding_dim).await?;
        }
        self.ensure_vector_index().await?;
        self.post_v2(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": COLLECTION }),
        )
        .await?;
        Ok(())
    }
}

// ── CollectionVectorStore ──────────────────────────────

fn field_to_milvus_type(ft: &str) -> &str {
    match ft {
        "int" | "int32" | "integer" => "Int32",
        "int64" | "bigint" | "long" => "Int64",
        "float" | "float32" => "Float",
        "float64" | "double" => "Double",
        "bool" | "boolean" => "Bool",
        _ => "VarChar",
    }
}

#[async_trait]
impl CollectionVectorStore for MilvusStore {
    async fn create_dynamic_collection(
        &self,
        name: &str,
        fields: &[FieldDefinition],
        embedding_dim: u32,
    ) -> Result<()> {
        let mut schema_fields = vec![
            json!({
                "fieldName": "id",
                "dataType": "VarChar",
                "isPrimary": true,
                "elementTypeParams": { "max_length": 64 }
            }),
            json!({
                "fieldName": "workspace_id",
                "dataType": "VarChar",
                "elementTypeParams": { "max_length": 64 }
            }),
        ];

        for f in fields {
            let dt = field_to_milvus_type(&f.field_type);
            let mut field = json!({
                "fieldName": f.name,
                "dataType": dt,
            });
            if dt == "VarChar" {
                field["elementTypeParams"] = json!({ "max_length": 65535 });
            }
            schema_fields.push(field);
        }

        schema_fields.push(json!({
            "fieldName": "vector",
            "dataType": "FloatVector",
            "elementTypeParams": { "dim": embedding_dim as i64 }
        }));

        let body = json!({
            "collectionName": name,
            "schema": {
                "enableDynamicField": false,
                "fields": schema_fields,
            }
        });
        self.post_v2("/v2/vectordb/collections/create", body)
            .await?;

        let idx = json!({
            "collectionName": name,
            "indexParams": [{
                "index_type": "AUTOINDEX",
                "metricType": "COSINE",
                "fieldName": "vector",
                "indexName": "vector"
            }]
        });
        match self.post_v2("/v2/vectordb/indexes/create", idx).await {
            Ok(_) => {}
            Err(e) => {
                let m = e.to_string();
                if !m.contains("same index name")
                    && !m.contains("IndexAlreadyExists")
                    && !m.contains("index already exist")
                {
                    return Err(e);
                }
            }
        }

        self.post_v2(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": name }),
        )
        .await?;
        Ok(())
    }

    async fn drop_dynamic_collection(&self, name: &str) -> Result<()> {
        self.post_v2(
            "/v2/vectordb/collections/drop",
            json!({ "collectionName": name }),
        )
        .await?;
        Ok(())
    }

    async fn insert_collection_rows(
        &self,
        collection_name: &str,
        workspace_id: &str,
        rows: &[serde_json::Value],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let data: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut row = r.clone();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert(
                        "workspace_id".to_string(),
                        Value::String(workspace_id.to_string()),
                    );
                }
                row
            })
            .collect();
        let body = json!({
            "collectionName": collection_name,
            "data": data
        });
        self.post_v2("/v2/vectordb/entities/insert", body).await?;
        Ok(())
    }

    async fn search_collection(
        &self,
        collection_name: &str,
        workspace_id: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let ws = milvus_quote(workspace_id);
        let filter = format!("workspace_id == {ws}");
        let lim = limit.min(16_383).max(1);
        let body = json!({
            "collectionName": collection_name,
            "data": [vector],
            "annsField": "vector",
            "filter": filter,
            "limit": lim,
            "outputFields": ["*"],
            "searchParams": { "metricType": "COSINE" }
        });
        let v = self.post_v2("/v2/vectordb/entities/search", body).await?;
        Ok(flatten_entity_rows(v.get("data")))
    }

    async fn query_collection(
        &self,
        collection_name: &str,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let ws = milvus_quote(workspace_id);
        let filter = format!("workspace_id == {ws}");
        let lim = limit.min(16_383).max(1);
        let body = json!({
            "collectionName": collection_name,
            "filter": filter,
            "limit": lim,
            "outputFields": ["*"]
        });
        let v = self.post_v2("/v2/vectordb/entities/query", body).await?;
        Ok(flatten_entity_rows(v.get("data")))
    }
}

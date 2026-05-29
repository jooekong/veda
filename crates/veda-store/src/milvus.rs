//! Milvus REST v2 `VectorStore` implementation.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tracing::warn;
use veda_core::store::{CollectionVectorStore, VectorStore, VectorWorkspaceStore};
use veda_types::{
    ChunkWithEmbedding, FieldDefinition, Result, SearchHit, SearchMode, SearchRequest,
    SummaryWithEmbedding, VectorRecordHit, VectorSearchHit, VedaError,
};

use std::time::Duration;

const COLLECTION: &str = "veda_chunks";
const SUMMARY_COLLECTION: &str = "veda_summaries";
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 300;

fn storage_err(e: impl ToString) -> VedaError {
    VedaError::Storage(e.to_string())
}

/// Compute the per-workspace default Milvus collection name for db-kind workspaces.
/// Format: `ws_<16-hex-chars-of-sha256(workspace_id)>_default`.
/// 16 hex = 64-bit hash space → collision probability is negligible even at
/// the plan's 1500-workspace target. 8 hex (32-bit) was the v1 of this code;
/// at 1500 ws the birthday-paradox probability was ~3e-4 and the "DB check
/// uniq" the plan mentioned was never actually wired.
/// The `_default` suffix distinguishes from v1 dedicated collections
/// (named `ws_<ws>_<dataset>_dim<DIM>_v<VER>` per docs/vectors-merge-plan.md §2.6).
/// Hash is over workspace.id (UUID), not workspace.name — workspace
/// rename is safe and never affects the underlying Milvus collection.
pub fn vector_collection_name(workspace_id: &str) -> String {
    let hash = veda_core::checksum::sha256_hex(workspace_id.as_bytes());
    format!("ws_{}_default", &hash[..16])
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

    /// Retry-enabled POST. Use ONLY for idempotent endpoints
    /// (search/query/upsert/delete/load/list/has/drop). Non-idempotent
    /// mutations (collections/create, entities/insert) MUST go through
    /// `post_no_retry` — replaying after a commit-then-timeout produces
    /// duplicate rows or AlreadyExists errors masking the real state.
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            match self.post_once(path, body.clone()).await {
                Ok(v) => return Ok(v),
                Err((e, retryable)) => {
                    if !retryable || attempt == MAX_RETRIES {
                        return Err(e);
                    }
                    let backoff_ms = BASE_BACKOFF_MS * 2u64.pow(attempt);
                    warn!(attempt, backoff_ms, path, err = %e, "milvus request failed, retrying");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap())
    }

    /// Single-shot POST without retry. Required for non-idempotent mutations
    /// where a transport-level retry could replay an already-committed write.
    async fn post_no_retry(&self, path: &str, body: Value) -> Result<Value> {
        self.post_once(path, body).await.map_err(|(e, _)| e)
    }

    async fn post_once(&self, path: &str, mut body: Value) -> std::result::Result<Value, (VedaError, bool)> {
        self.inject_db(&mut body);
        let resp = self
            .http
            .post(self.url(path))
            .headers(self.headers().map_err(|e| (e, false))?)
            .json(&body)
            .send()
            .await
            .map_err(|e| (VedaError::Storage(e.to_string()), true))?;
        let status = resp.status();
        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        let text = resp
            .text()
            .await
            .map_err(|e| (VedaError::Storage(e.to_string()), true))?;
        if retryable {
            return Err((
                VedaError::Storage(format!("milvus HTTP {status}: {text}")),
                true,
            ));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            (VedaError::Storage(format!(
                "milvus invalid json (HTTP {status}): {e}; body: {text}"
            )), false)
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
            return Err((VedaError::Storage(format!(
                "milvus error code {code}: {msg}"
            )), false));
        }
        Ok(v)
    }

    async fn collection_exists(&self) -> Result<bool> {
        let v = self
            .post(
                "/v2/vectordb/collections/has",
                json!({ "collectionName": COLLECTION }),
            )
            .await?;
        Ok(v["data"]["has"].as_bool().unwrap_or(false))
    }

    async fn create_collection(&self, embedding_dim: u32) -> Result<()> {
        let dim = embedding_dim as i64;
        // Schema v2: adds `sparse_vector` field + BM25 function over `content`
        // so hybrid_search can fuse two real ranking signals (dense ANN +
        // sparse BM25) instead of just RRF-wrapping a single dense source.
        // Requires Milvus 2.5+ (BM25 function landed in 2.5; deployed 2.6.14).
        // `enable_analyzer` on content is what lets
        // the BM25 function tokenize on insert.
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
                        "elementTypeParams": {
                            "max_length": 65535,
                            "enable_analyzer": true,
                            // Veda content is heavily mixed Chinese / English.
                            // `standard` does whitespace+punct splitting, which
                            // for Chinese yields one-token-per-sentence — BM25
                            // becomes ~useless. `jieba` segments Chinese AND
                            // falls back for ASCII, which gives sane BM25 over
                            // both. Requires Milvus built with jieba support
                            // (default milvusdb/milvus:latest has it).
                            "analyzer_params": { "tokenizer": "jieba" }
                        }
                    },
                    {
                        "fieldName": "vector",
                        "dataType": "FloatVector",
                        "elementTypeParams": { "dim": dim }
                    },
                    {
                        "fieldName": "sparse_vector",
                        "dataType": "SparseFloatVector"
                    }
                ],
                "functions": [
                    {
                        "name": "bm25_content",
                        "type": "BM25",
                        "inputFieldNames": ["content"],
                        "outputFieldNames": ["sparse_vector"]
                    }
                ]
            }
        });
        self.post_no_retry("/v2/vectordb/collections/create", body)
            .await?;
        Ok(())
    }

    async fn ensure_vector_index(&self) -> Result<()> {
        // Two indexes can't be created in one POST when one is AUTOINDEX
        // (Milvus 2.5 rejects extra params on the AUTOINDEX entry). Issue
        // them separately.
        for body in [
            json!({
                "collectionName": COLLECTION,
                "indexParams": [{
                    "index_type": "AUTOINDEX",
                    "metricType": "COSINE",
                    "fieldName": "vector",
                    "indexName": "vector"
                }]
            }),
            json!({
                "collectionName": COLLECTION,
                "indexParams": [{
                    "index_type": "SPARSE_INVERTED_INDEX",
                    "metricType": "BM25",
                    "fieldName": "sparse_vector",
                    "indexName": "sparse_vector"
                }]
            }),
        ] {
            match self.post("/v2/vectordb/indexes/create", body).await {
                Ok(_) => {}
                Err(e) => {
                    let m = e.to_string();
                    if m.contains("same index name")
                        || m.contains("IndexAlreadyExists")
                        || m.contains("index already exist")
                    {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    async fn init_summary_collection(&self, embedding_dim: u32) -> Result<()> {
        let has = self
            .post(
                "/v2/vectordb/collections/has",
                json!({ "collectionName": SUMMARY_COLLECTION }),
            )
            .await?;
        if !has["data"]["has"].as_bool().unwrap_or(false) {
            let dim = embedding_dim as i64;
            let body = json!({
                "collectionName": SUMMARY_COLLECTION,
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
                            "fieldName": "summary_type",
                            "dataType": "VarChar",
                            "elementTypeParams": { "max_length": 16 }
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
            self.post_no_retry("/v2/vectordb/collections/create", body)
                .await?;

            let idx = json!({
                "collectionName": SUMMARY_COLLECTION,
                "indexParams": [{
                    "index_type": "AUTOINDEX",
                    "metricType": "COSINE",
                    "fieldName": "vector",
                    "indexName": "vector"
                }]
            });
            match self.post("/v2/vectordb/indexes/create", idx).await {
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
        }
        self.post(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": SUMMARY_COLLECTION }),
        )
        .await?;
        Ok(())
    }

    /// Create a Pinecone-style vector collection for a db-kind workspace.
    /// Schema follows docs/vectors-merge-plan.md §2.2:
    ///   - composite PK `{dataset}:{id}` (Milvus PK enforces upsert dedup)
    ///   - 3-tier classification: dataset / category / tags (all default-friendly)
    ///   - row-level status, created_at/updated_at, optional expire_at
    ///   - hybrid: dense `vector` + BM25 `sparse_vector`
    ///   - free-form `meta` JSON
    ///
    /// Indexes v0: vector AUTOINDEX COSINE, sparse_vector SPARSE_INVERTED_INDEX BM25,
    /// scalar INVERTED on dataset/category/tags/status/created_at.
    /// id/updated_at/expire_at are schema-only (no index v0, defer to v1).
    ///
    /// PK immutable contract: workspace rename is safe (collection name is hashed
    /// from workspace.id), but dataset rename requires data migration.
    pub async fn create_vector_collection(
        &self,
        workspace_id: &str,
        dim: u32,
    ) -> Result<String> {
        let name = vector_collection_name(workspace_id);
        let dim_i = dim as i64;
        let body = json!({
            "collectionName": &name,
            "schema": {
                "enableDynamicField": false,
                "fields": [
                    { "fieldName": "pk", "dataType": "VarChar", "isPrimary": true,
                      "elementTypeParams": { "max_length": 128 } },
                    { "fieldName": "id", "dataType": "VarChar",
                      "elementTypeParams": { "max_length": 128 } },
                    { "fieldName": "dataset", "dataType": "VarChar",
                      "elementTypeParams": { "max_length": 64 } },
                    { "fieldName": "category", "dataType": "VarChar",
                      "elementTypeParams": { "max_length": 64 } },
                    { "fieldName": "tags", "dataType": "Array",
                      "elementDataType": "VarChar",
                      "elementTypeParams": { "max_length": 128, "max_capacity": 8 } },
                    { "fieldName": "status", "dataType": "VarChar",
                      "elementTypeParams": { "max_length": 32 } },
                    { "fieldName": "created_at", "dataType": "Int64" },
                    { "fieldName": "updated_at", "dataType": "Int64" },
                    { "fieldName": "expire_at", "dataType": "Int64", "nullable": true },
                    { "fieldName": "text", "dataType": "VarChar",
                      "elementTypeParams": {
                          // max_length is UTF-8 bytes (per Milvus 2.6 FAQ,
                          // not characters despite some doc pages). 65535
                          // is the VARCHAR hard upper bound. Aligned with
                          // validate::TEXT_MAX_BYTES.
                          "max_length": 65535,
                          "enable_analyzer": true,
                          "analyzer_params": { "tokenizer": "jieba" }
                      } },
                    { "fieldName": "vector", "dataType": "FloatVector",
                      "elementTypeParams": { "dim": dim_i } },
                    { "fieldName": "sparse_vector", "dataType": "SparseFloatVector" },
                    { "fieldName": "meta", "dataType": "JSON" }
                ],
                "functions": [{
                    "name": "bm25_text",
                    "type": "BM25",
                    "inputFieldNames": ["text"],
                    "outputFieldNames": ["sparse_vector"]
                }]
            }
        });
        // Idempotent: if the collection already exists, fall through to ensure
        // indexes + load (both also idempotent). Lets caller retry safely after
        // a transient failure between collection create and Milvus ack.
        if let Err(e) = self
            .post_no_retry("/v2/vectordb/collections/create", body)
            .await
        {
            let m = e.to_string();
            if !m.contains("CollectionAlreadyExists")
                && !m.contains("collection already exist")
            {
                return Err(e);
            }
        }

        // 7 indexes: 1 vector + 1 sparse + 5 scalar inverted. Each is its own
        // POST — Milvus 2.5+ rejects multi-index POST with AUTOINDEX entry.
        let indexes = [
            ("vector", "AUTOINDEX", "COSINE"),
            ("sparse_vector", "SPARSE_INVERTED_INDEX", "BM25"),
            ("dataset", "INVERTED", ""),
            ("category", "INVERTED", ""),
            ("tags", "INVERTED", ""),
            ("status", "INVERTED", ""),
            ("created_at", "INVERTED", ""),
        ];
        for (field, idx_type, metric) in indexes {
            let mut params = json!({
                "index_type": idx_type,
                "fieldName": field,
                "indexName": field,
            });
            if !metric.is_empty() {
                params["metricType"] = json!(metric);
            }
            let body = json!({
                "collectionName": &name,
                "indexParams": [params],
            });
            match self.post("/v2/vectordb/indexes/create", body).await {
                Ok(_) => {}
                Err(e) => {
                    let m = e.to_string();
                    if m.contains("same index name")
                        || m.contains("IndexAlreadyExists")
                        || m.contains("index already exist")
                    {
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        self.post(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": &name }),
        )
        .await?;

        Ok(name)
    }

    /// Drop a Milvus collection by name. Used by Stage 2.2 provisioner for
    /// rollback on partial-failure. Idempotent: returns Ok if the collection
    /// doesn't exist.
    pub async fn drop_collection(&self, name: &str) -> Result<()> {
        match self
            .post(
                "/v2/vectordb/collections/drop",
                json!({ "collectionName": name }),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let m = e.to_string();
                if m.contains("CollectionNotExists")
                    || m.contains("collection not exist")
                    || m.contains("can't find collection")
                {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Output fields returned for every search/query hit on db-workspace
    /// collections. `vector` and `sparse_vector` are intentionally absent
    /// (no client needs raw embeddings back from the server). `pk` /
    /// `status` / `expire_at` are columns in the Milvus schema but
    /// deliberately not surfaced via the API (composite pk is internal;
    /// status pins to "active" via the base filter; expire_at is schema-
    /// only until v1 ships TTL).
    fn vector_output_fields(selected: Option<&[String]>) -> Vec<String> {
        match selected {
            // Default: id + all projectable fields (the old fixed list).
            None => [
                "id",
                "dataset",
                "category",
                "tags",
                "text",
                "meta",
                "created_at",
                "updated_at",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            // Projection: id is always fetched (correlation + always
            // returned); append the caller-selected projectable fields.
            Some(sel) => {
                let mut v = vec!["id".to_string()];
                v.extend(sel.iter().cloned());
                v
            }
        }
    }

    /// Build a Milvus filter expression that scopes a query to one dataset
    /// and (by default) active rows. Stage 4.4 will introduce caller-side
    /// filters that this baseline AND-merges with.
    fn build_dataset_active_filter(dataset: &str) -> String {
        format!(
            "dataset == {} && status == \"active\"",
            milvus_quote(dataset)
        )
    }

    /// Build `pk in [...]` for query and delete by composite PK.
    fn build_pk_in_filter(pks: &[String]) -> String {
        let inner: Vec<String> = pks.iter().map(|p| milvus_quote(p)).collect();
        format!("pk in [{}]", inner.join(","))
    }

    /// Upsert a batch of records into a db-workspace collection.
    /// Caller-supplied `UpsertRecord` is the source of truth — defaults
    /// already filled, `pk` already composed, dense `vector` already
    /// computed. `sparse_vector` is NOT in the payload: Milvus 2.5+ runs
    /// the BM25 function on `text` automatically.
    pub async fn upsert_vector_records(
        &self,
        workspace_id: &str,
        records: &[veda_types::UpsertRecord],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let name = vector_collection_name(workspace_id);
        let data: Vec<Value> = records
            .iter()
            .map(|r| {
                // status is hardcoded "active" because v0 doesn't surface
                // status as a public API field; the base search filter
                // pins to active anyway. expire_at is omitted (v0 has no
                // TTL feature; column stays nullable in Milvus schema).
                json!({
                    "pk": r.pk,
                    "id": r.id,
                    "dataset": r.dataset,
                    "category": r.category,
                    "tags": r.tags,
                    "status": "active",
                    "created_at": r.created_at,
                    "updated_at": r.updated_at,
                    "text": r.text,
                    "vector": r.vector,
                    "meta": r.meta,
                })
            })
            .collect();
        let body = json!({ "collectionName": &name, "data": data });
        self.post("/v2/vectordb/entities/upsert", body).await?;
        Ok(())
    }

    /// Map one Milvus row (from search/get response `data`) to a record hit
    /// without score. Caller of search adds the distance separately.
    ///
    /// Required fields (id/dataset/category/text/created_at/updated_at) are
    /// extracted with explicit error — `outputFields` already asked Milvus
    /// for these, so a missing field signals schema drift or a REST
    /// contract change, not normal data. Silently returning empty strings
    /// (the v1 of this code) hid those bugs as "successful" hits with all
    /// blank fields. Optional fields (tags, meta) keep their default-on-
    /// missing behavior — those are nullable by design.
    fn row_to_vector_record_hit(row: &Value) -> Result<VectorRecordHit> {
        // `id` is always in outputFields (always fetched/returned). Every
        // other field is present only when not projected out, so map
        // present → Some / absent → None to honor the caller's projection.
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| VedaError::Storage("Milvus hit missing required field: id".into()))?;
        let opt_str = |k: &str| row.get(k).and_then(|v| v.as_str()).map(String::from);
        let opt_i64 = |k: &str| row.get(k).and_then(|v| v.as_i64());
        // Milvus 2.6 REST quirks discovered via Stage 4 data-plane test:
        // - Array<VarChar> returns nested as `{"Data":{"StringData":{"data":[...]}}}`,
        //   not a flat JSON array. Traverse the wrapper.
        // - JSON column returns as a JSON-encoded string (not parsed). Re-parse.
        // Field present (even if empty) → Some(default); projected out → None.
        let tags = row.get("tags").map(|v| {
            v.get("Data")
                .and_then(|v| v.get("StringData"))
                .and_then(|v| v.get("data"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        });
        let meta = row.get("meta").map(|v| {
            v.as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}))
        });
        Ok(VectorRecordHit {
            id,
            dataset: opt_str("dataset"),
            category: opt_str("category"),
            tags,
            text: opt_str("text"),
            meta,
            created_at: opt_i64("created_at"),
            updated_at: opt_i64("updated_at"),
        })
    }

    pub async fn search_vector_collection(
        &self,
        workspace_id: &str,
        dataset: &str,
        query_vector: &[f32],
        top_k: usize,
        extra_filter: Option<&str>,
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorSearchHit>> {
        let name = vector_collection_name(workspace_id);
        let base = Self::build_dataset_active_filter(dataset);
        // AND-merge the caller's parsed Filter DSL (Stage 4.4) with the base.
        // None / empty extra → just base.
        let filter = match extra_filter {
            Some(s) if !s.is_empty() => format!("({base}) && ({s})"),
            _ => base,
        };
        let body = json!({
            "collectionName": &name,
            "data": [query_vector],
            "annsField": "vector",
            "filter": filter,
            "limit": top_k,
            "outputFields": Self::vector_output_fields(output_fields),
            "searchParams": { "metricType": "COSINE" },
            // Strong consistency so a search right after upsert sees the
            // write — the upsert commit_ts contract promises read-your-writes,
            // and the default Bounded level would silently break it (all fs
            // read paths use Strong for the same reason).
            "consistencyLevel": "Strong",
        });
        let resp = self.post("/v2/vectordb/entities/search", body).await?;
        let rows = flatten_entity_rows(resp.get("data"));
        rows.iter()
            .map(|row| {
                let base = Self::row_to_vector_record_hit(row)?;
                let score = row
                    .get("distance")
                    .and_then(|v| v.as_f64())
                    .map(|d| d as f32)
                    .unwrap_or(0.0);
                Ok(VectorSearchHit {
                    id: base.id,
                    dataset: base.dataset,
                    category: base.category,
                    tags: base.tags,
                    text: base.text,
                    meta: base.meta,
                    created_at: base.created_at,
                    updated_at: base.updated_at,
                    score,
                })
            })
            .collect()
    }

    pub async fn query_vector_records_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorRecordHit>> {
        if pks.is_empty() {
            return Ok(Vec::new());
        }
        let name = vector_collection_name(workspace_id);
        // Use entities/query with `pk in [...]` instead of entities/get:
        // get doesn't carry a consistencyLevel, and we need Strong here so a
        // query right after upsert sees the write (read-your-writes, matching
        // the commit_ts contract). `limit` must cover the whole pk batch
        // (caller caps it at MAX_PK_BATCH = 500).
        let pk_list = pks
            .iter()
            .map(|p| milvus_quote(p))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = format!("pk in [{pk_list}]");
        let body = json!({
            "collectionName": &name,
            "filter": filter,
            "limit": pks.len(),
            "outputFields": Self::vector_output_fields(output_fields),
            "consistencyLevel": "Strong",
        });
        let resp = self.post("/v2/vectordb/entities/query", body).await?;
        let rows = flatten_entity_rows(resp.get("data"));
        rows.iter().map(Self::row_to_vector_record_hit).collect()
    }

    pub async fn delete_vector_records_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
    ) -> Result<usize> {
        if pks.is_empty() {
            return Ok(0);
        }
        let name = vector_collection_name(workspace_id);
        let filter = Self::build_pk_in_filter(pks);
        let body = json!({
            "collectionName": &name,
            "filter": filter,
        });
        let resp = self.post("/v2/vectordb/entities/delete", body).await?;
        // Milvus 2.6 REST returns `data.deleteCount` — the number of delete
        // markers the engine created for the filter. For our `pk in [...]`
        // filter this equals `pks.len()` regardless of whether each pk
        // physically existed (Milvus creates a tombstone per matched PK
        // expression term). Documented in docs/api/vectors.md so callers
        // don't mistake it for "rows that existed and were removed".
        //
        // Fall back to `pks.len()` if the field is missing (defensive
        // against future REST gateway shape changes; current 2.6.14
        // always returns it — verified by sub_full_roundtrip).
        let count = match resp
            .get("data")
            .and_then(|d| d.get("deleteCount"))
            .and_then(|n| n.as_u64())
        {
            Some(n) => n as usize,
            None => {
                // Should not happen on Milvus 2.6.14 — log so a future
                // REST gateway shape change surfaces in ops dashboards
                // instead of silently degrading the published contract.
                warn!(
                    response = %resp,
                    "Milvus delete response missing data.deleteCount; falling back to submitted count"
                );
                pks.len()
            }
        };
        Ok(count)
    }

    fn summary_rows_to_hits(rows: &[Value], limit: usize) -> Vec<SearchHit> {
        rows.iter()
            .take(limit)
            .map(|row| {
                let id = row.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let content = row.get("content").and_then(|x| x.as_str()).unwrap_or("");
                let score = row
                    .get("distance")
                    .and_then(|x| x.as_f64())
                    .map(|d| d as f32)
                    .or_else(|| row.get("score").and_then(|x| x.as_f64()).map(|d| d as f32))
                    .unwrap_or(0.0);
                SearchHit {
                    file_id: id.to_string(),
                    chunk_index: None,
                    content: content.to_string(),
                    score,
                    score_type: "cosine".to_string(),
                    path: None,
                    l0_abstract: Some(content.to_string()),
                    l1_overview: None,
                }
            })
            .collect()
    }

    fn rows_to_hits(rows: &[Value], limit: usize, score_type: &str) -> Vec<SearchHit> {
        let mut out = Vec::new();
        for row in rows.iter().take(limit) {
            let file_id = row
                .get("file_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let chunk_index = row.get("chunk_index").and_then(|x| x.as_i64()).map(|x| x as i32);
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
                score_type: score_type.to_string(),
                path: None,
                l0_abstract: None,
                l1_overview: None,
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
            "searchParams": { "metricType": "COSINE" },
            "consistencyLevel": "Strong"
        });
        let v = self.post("/v2/vectordb/entities/search", body).await?;
        let rows = flatten_entity_rows(v.get("data"));
        Ok(Self::rows_to_hits(&rows, limit, "cosine"))
    }

    async fn hybrid_search_remote(&self, req: &SearchRequest) -> Result<Option<Vec<SearchHit>>> {
        let qv = req.query_vector.as_ref().unwrap();
        let ws = milvus_quote(&req.workspace_id);
        let base_filter = format!("workspace_id == {ws}");
        let lim = req.limit.min(16_383).max(1);
        // True hybrid: two real rankers fused by RRF.
        //   1. dense ANN over `vector` (semantic similarity)
        //   2. BM25 over `sparse_vector` (lexical relevance)
        // Sending only the dense object (as the previous code did) made
        // RRF a no-op identity reorder — hybrid degenerated into semantic.
        let dense = json!({
            "data": [qv],
            "annsField": "vector",
            "filter": base_filter.clone(),
            "limit": lim,
            "offset": 0,
            "ignoreGrowing": false,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"],
            "metricType": "COSINE"
        });
        let bm25 = json!({
            "data": [req.query.clone()],
            "annsField": "sparse_vector",
            "filter": base_filter,
            "limit": lim,
            "offset": 0,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"],
            "metricType": "BM25"
        });
        let body = json!({
            "collectionName": COLLECTION,
            "search": [dense, bm25],
            "rerank": {
                "strategy": "rrf",
                "params": { "k": 60 }
            },
            "limit": lim,
            "outputFields": ["id", "workspace_id", "file_id", "chunk_index", "content"],
            "consistencyLevel": "Strong"
        });
        match self.post("/v2/vectordb/entities/hybrid_search", body).await {
            Ok(v) => {
                let rows = flatten_entity_rows(v.get("data"));
                Ok(Some(Self::rows_to_hits(&rows, req.limit, "rrf")))
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
        // Real BM25 over the sparse_vector field (Milvus tokenizes via the
        // BM25 function defined in the schema). Replaces the previous LIKE
        // '%query%' substring scan, which had no ranking and didn't match
        // the README's "BM25 keyword" claim. For literal-substring needs
        // (with line numbers), use `veda grep` instead.
        let ws = milvus_quote(workspace_id);
        let lim = limit.min(16_383).max(1);
        let body = json!({
            "collectionName": COLLECTION,
            "data": [query],
            "annsField": "sparse_vector",
            "filter": format!("workspace_id == {ws}"),
            "limit": lim,
            "outputFields": ["id", "file_id", "chunk_index", "content"],
            "metricType": "BM25",
            "consistencyLevel": "Strong"
        });
        let v = self.post("/v2/vectordb/entities/search", body).await?;
        let rows = flatten_entity_rows(v.get("data"));
        Ok(Self::rows_to_hits(&rows, limit, "bm25"))
    }
}

#[async_trait]
impl VectorStore for MilvusStore {
    async fn ping(&self) -> Result<()> {
        self.post("/v2/vectordb/collections/list", json!({}))
            .await?;
        Ok(())
    }

    async fn upsert_chunks(&self, chunks: &[ChunkWithEmbedding]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        // Whole-file path: upsert + sweep trailing stale. Safe when all
        // chunks of the file are passed in a single call.
        self.upsert_chunks_only(chunks).await?;
        let max_index = chunks.iter().map(|c| c.chunk_index).max().unwrap_or(0);
        let file_id = &chunks[0].file_id;
        let ws_id = &chunks[0].workspace_id;
        self.delete_chunks_above(ws_id, file_id, max_index).await
    }

    async fn upsert_chunks_only(&self, chunks: &[ChunkWithEmbedding]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
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
        self.post("/v2/vectordb/entities/upsert", body).await?;
        Ok(())
    }

    async fn delete_chunks_above(
        &self,
        workspace_id: &str,
        file_id: &str,
        max_chunk_index: i32,
    ) -> Result<()> {
        let ws = milvus_quote(workspace_id);
        let fid = milvus_quote(file_id);
        let filter = format!(
            "workspace_id == {ws} && file_id == {fid} && chunk_index > {max_chunk_index}"
        );
        let body = json!({
            "collectionName": COLLECTION,
            "filter": filter
        });
        self.post("/v2/vectordb/entities/delete", body).await?;
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
        self.post("/v2/vectordb/entities/delete", body).await?;
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

    async fn upsert_summaries(&self, summaries: &[SummaryWithEmbedding]) -> Result<()> {
        if summaries.is_empty() {
            return Ok(());
        }
        let data: Vec<Value> = summaries
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "workspace_id": s.workspace_id,
                    "summary_type": s.summary_type,
                    "content": s.content,
                    "vector": s.vector
                })
            })
            .collect();
        let body = json!({
            "collectionName": SUMMARY_COLLECTION,
            "data": data
        });
        self.post("/v2/vectordb/entities/upsert", body).await?;
        Ok(())
    }

    async fn delete_summary(&self, workspace_id: &str, id: &str) -> Result<()> {
        let ws = milvus_quote(workspace_id);
        let sid = milvus_quote(id);
        let filter = format!("workspace_id == {ws} && id == {sid}");
        let body = json!({
            "collectionName": SUMMARY_COLLECTION,
            "filter": filter
        });
        self.post("/v2/vectordb/entities/delete", body).await?;
        Ok(())
    }

    async fn search_summaries(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        let ws = milvus_quote(&req.workspace_id);
        let filter = format!("workspace_id == {ws}");
        let lim = req.limit.min(16_383).max(1);

        match &req.query_vector {
            Some(v) if !v.is_empty() => {
                let body = json!({
                    "collectionName": SUMMARY_COLLECTION,
                    "data": [v],
                    "annsField": "vector",
                    "filter": filter,
                    "limit": lim,
                    "outputFields": ["id", "workspace_id", "summary_type", "content"],
                    "searchParams": { "metricType": "COSINE" },
                    "consistencyLevel": "Strong"
                });
                let v = self.post("/v2/vectordb/entities/search", body).await?;
                let rows = flatten_entity_rows(v.get("data"));
                Ok(Self::summary_rows_to_hits(&rows, req.limit))
            }
            _ => Ok(vec![]),
        }
    }

    async fn list_chunk_file_ids(&self, workspace_id: &str) -> Result<Vec<String>> {
        let ws = milvus_quote(workspace_id);
        let filter = format!("workspace_id == {ws}");
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Milvus has no DISTINCT — paginate the chunks and dedupe client-side.
        // 16383 is the documented hard upper bound for `limit` on entities/query.
        let page_size = 16_383i64;
        let mut offset: i64 = 0;
        loop {
            let body = json!({
                "collectionName": COLLECTION,
                "filter": filter,
                "limit": page_size,
                "offset": offset,
                "outputFields": ["file_id"],
                "consistencyLevel": "Strong"
            });
            let v = self.post("/v2/vectordb/entities/query", body).await?;
            let rows = flatten_entity_rows(v.get("data"));
            let n = rows.len();
            for row in &rows {
                if let Some(fid) = row.get("file_id").and_then(|x| x.as_str()) {
                    seen.insert(fid.to_string());
                }
            }
            if (n as i64) < page_size {
                break;
            }
            offset += page_size;
        }
        Ok(seen.into_iter().collect())
    }

    async fn list_summary_ids(&self, workspace_id: &str) -> Result<Vec<String>> {
        let ws = milvus_quote(workspace_id);
        let filter = format!("workspace_id == {ws}");
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let page_size = 16_383i64;
        let mut offset: i64 = 0;
        loop {
            let body = json!({
                "collectionName": SUMMARY_COLLECTION,
                "filter": filter,
                "limit": page_size,
                "offset": offset,
                "outputFields": ["id"],
                "consistencyLevel": "Strong"
            });
            let v = self.post("/v2/vectordb/entities/query", body).await?;
            let rows = flatten_entity_rows(v.get("data"));
            let n = rows.len();
            for row in &rows {
                if let Some(id) = row.get("id").and_then(|x| x.as_str()) {
                    seen.insert(id.to_string());
                }
            }
            if (n as i64) < page_size {
                break;
            }
            offset += page_size;
        }
        Ok(seen.into_iter().collect())
    }

    async fn init_collections(&self, embedding_dim: u32) -> Result<()> {
        if !self.collection_exists().await? {
            self.create_collection(embedding_dim).await?;
        }
        self.ensure_vector_index().await?;
        self.post(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": COLLECTION }),
        )
        .await?;

        self.init_summary_collection(embedding_dim).await?;
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
impl VectorWorkspaceStore for MilvusStore {
    async fn create_vector_collection(
        &self,
        workspace_id: &str,
        dim: u32,
    ) -> Result<String> {
        // Delegate to the inherent method. The trait keeps the
        // collection-management API testable behind a stub for non-Milvus
        // backends (future) and avoids forcing AppState to carry a concrete
        // `Arc<MilvusStore>` alongside `Arc<dyn VectorStore>`.
        MilvusStore::create_vector_collection(self, workspace_id, dim).await
    }

    async fn drop_collection(&self, name: &str) -> Result<()> {
        MilvusStore::drop_collection(self, name).await
    }

    async fn upsert_records(
        &self,
        workspace_id: &str,
        records: &[veda_types::UpsertRecord],
    ) -> Result<i64> {
        MilvusStore::upsert_vector_records(self, workspace_id, records).await?;
        // Milvus REST `/v2/vectordb/entities/upsert` doesn't return a true
        // commit_ts. Under synchronous semantics (no outbox), the call
        // returns after Milvus acked → server-now is a valid stand-in for
        // read-your-writes on the same instance.
        Ok(chrono::Utc::now().timestamp_millis())
    }

    async fn search_vectors(
        &self,
        workspace_id: &str,
        dataset: &str,
        query_vector: &[f32],
        top_k: usize,
        extra_filter: Option<&str>,
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorSearchHit>> {
        MilvusStore::search_vector_collection(
            self,
            workspace_id,
            dataset,
            query_vector,
            top_k,
            extra_filter,
            output_fields,
        )
        .await
    }

    async fn query_vectors_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
        output_fields: Option<&[String]>,
    ) -> Result<Vec<VectorRecordHit>> {
        MilvusStore::query_vector_records_by_pk(self, workspace_id, pks, output_fields).await
    }

    async fn delete_vectors_by_pk(
        &self,
        workspace_id: &str,
        pks: &[String],
    ) -> Result<usize> {
        MilvusStore::delete_vector_records_by_pk(self, workspace_id, pks).await
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
        self.post_no_retry("/v2/vectordb/collections/create", body)
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
        match self.post("/v2/vectordb/indexes/create", idx).await {
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

        self.post(
            "/v2/vectordb/collections/load",
            json!({ "collectionName": name }),
        )
        .await?;
        Ok(())
    }

    async fn drop_dynamic_collection(&self, name: &str) -> Result<()> {
        self.post(
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
        self.post_no_retry("/v2/vectordb/entities/insert", body)
            .await?;
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
            "searchParams": { "metricType": "COSINE" },
            "consistencyLevel": "Strong"
        });
        let v = self.post("/v2/vectordb/entities/search", body).await?;
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
            "outputFields": ["*"],
            "consistencyLevel": "Strong"
        });
        let v = self.post("/v2/vectordb/entities/query", body).await?;
        Ok(flatten_entity_rows(v.get("data")))
    }
}

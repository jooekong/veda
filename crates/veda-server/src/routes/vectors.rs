//! Pinecone-style vectors data plane (db-kind workspaces only).
//!
//! Stage 4.2 lands `POST /v1/vectors/upsert` only. Stage 4.3 will add
//! search / query / delete in the same module.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use veda_types::api::{
    NewRecord, UpsertRequest, UpsertResponse, VectorDeleteRequest, VectorDeleteResponse,
    VectorQueryRequest, VectorQueryResponse, VectorSearchRequest, VectorSearchResponse,
};
use veda_types::{
    validate, ApiResponse, SearchMode, UpsertRecord, VectorSearchQuery, VedaError, WriteMode,
};

use crate::auth::AuthDbWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/vectors/upsert", post(upsert_vectors))
        .route("/v1/vectors/search", post(search_vectors))
        .route("/v1/vectors/query", post(query_vectors))
        .route("/v1/vectors/delete", post(delete_vectors))
        // These endpoints accept bulk bodies far over axum's 2MB default: a
        // documented 500-record upsert (text <=64KB + meta <=16KB each) is
        // ~40MB. Without this the body-limit layer 413s such a request with a
        // bare error before the handler's structured PayloadTooLarge check.
        .layer(DefaultBodyLimit::max(MAX_BODY_MB * 1024 * 1024))
}

/// top_k default + ceiling from plan §3.2 / vss design.
const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;

/// Batch cap matches plan §3.2 limit; oversized batches return 413 (via
/// `VedaError::PayloadTooLarge`) instead of letting Milvus / embedding
/// upstream reject opaquely. Matches vss `openapi.yaml` Error413 contract.
const MAX_RECORDS_PER_UPSERT: usize = 500;

/// HTTP request-body ceiling for the bulk vectors endpoints. Sized to admit
/// a max MAX_RECORDS_PER_UPSERT batch plus JSON overhead; far above axum's
/// 2MB default (which would otherwise 413 a legitimate large upsert).
const MAX_BODY_MB: usize = 64;

async fn upsert_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<ApiResponse<UpsertResponse>>, AppError> {
    auth.require_write()?;
    let resp = do_upsert(&state, &auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Core upsert: validate → dedupe → embed → write. Shared by the `wk_` data
/// plane (`upsert_vectors` above) and the platform-gateway surface
/// (`project_data.rs`). The caller enforces write permission (wk_ via
/// `require_write`, gateway via external authz).
pub(crate) async fn do_upsert(
    state: &AppState,
    workspace_id: &str,
    req: UpsertRequest,
) -> Result<UpsertResponse, AppError> {
    let mut timer = VectorReqTimer::start(
        "upsert",
        workspace_id,
        req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        "none",
    );
    if req.records.is_empty() {
        return Err(VedaError::InvalidInput("records must not be empty".into()).into());
    }
    if req.records.len() > MAX_RECORDS_PER_UPSERT {
        return Err(VedaError::PayloadTooLarge(format!(
            "records: {} exceeds {MAX_RECORDS_PER_UPSERT}",
            req.records.len()
        ))
        .into());
    }

    let dataset_name =
        resolve_dataset(state, workspace_id, req.dataset.as_deref()).await?;
    timer.set_dataset(&dataset_name);

    // 4. Validate every record and resolve defaults BEFORE the embedding
    //    call — embedding is the expensive step, no point spending it on
    //    a batch we'll reject later. Each record is normalized into
    //    `(id, NormalizedFields)` plus the text to embed.
    let now_ms = Utc::now().timestamp_millis();
    let mut normalized: Vec<NormalizedRecord> = Vec::with_capacity(req.records.len());
    for rec in &req.records {
        normalized.push(normalize_record(rec, &dataset_name)?);
    }

    // 5. Server-side dedupe by id, last-wins. Milvus 2.6 rejects same-
    //    batch duplicate PKs (error code 1100), so we collapse here to
    //    deliver the wire contract documented in docs/api/vectors.md
    //    "Idempotency": "the latest occurrence in the records array
    //    wins; earlier occurrences are dropped before embedding". Done
    //    BEFORE embedding so we don't pay for vectors we'll discard.
    //
    //    Algorithm: walk forward, track first-seen index per id; on
    //    collision, overwrite in place to keep position of first
    //    occurrence with the value of last. Auto-generated UUIDs
    //    (omitted id case) are random v4 — collision is negligible.
    let mut seen_at: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(normalized.len());
    let mut deduped: Vec<NormalizedRecord> = Vec::with_capacity(normalized.len());
    for nr in normalized {
        match seen_at.get(&nr.id).copied() {
            Some(idx) => deduped[idx] = nr,
            None => {
                seen_at.insert(nr.id.clone(), deduped.len());
                deduped.push(nr);
            }
        }
    }

    // 6. Batched embed — one upstream call (cache hits skip).
    let texts: Vec<String> = deduped.iter().map(|nr| nr.text.clone()).collect();
    let vectors = state.vector_embedding.embed(&texts).await?;
    if vectors.len() != deduped.len() {
        // EmbeddingCache + EmbeddingProvider both guarantee 1-to-1; treat
        // any mismatch as an upstream contract violation, not silent loss.
        return Err(VedaError::EmbeddingFailed(format!(
            "embedded {} vectors for {} records",
            vectors.len(),
            deduped.len()
        ))
        .into());
    }

    // 7. Build Milvus payload records, routing each to the upsert or insert
    //    batch. write_mode=insert → all insert; write_mode=upsert (default)
    //    → explicit-id records upsert (idempotent), id-less records take the
    //    insert fast path (a UUID can't collide; skips Milvus's ~400ms dedup).
    let write_mode = req.write_mode;
    let mut upsert_batch: Vec<UpsertRecord> = Vec::new();
    let mut insert_batch: Vec<UpsertRecord> = Vec::new();
    let mut ids: Vec<String> = Vec::with_capacity(deduped.len());
    for (nr, vec) in deduped.into_iter().zip(vectors.into_iter()) {
        ids.push(nr.id.clone());
        let use_insert = write_mode == WriteMode::Insert || !nr.had_explicit_id;
        let record = UpsertRecord {
            pk: nr.pk,
            id: nr.id,
            dataset: nr.dataset,
            category: nr.category,
            tags: nr.tags,
            text: nr.text,
            vector: vec,
            meta: nr.meta,
            created_at: now_ms,
            updated_at: now_ms,
        };
        if use_insert {
            insert_batch.push(record);
        } else {
            upsert_batch.push(record);
        }
    }

    // 8. Submit. A mixed batch issues two Milvus calls and is NOT atomic
    //    (see docs/archive/plans/vector-write-mode-plan.md "重试与原子性契约").
    //    commit_ts is server-now (Milvus REST doesn't surface a real one).
    let mut commit_ts = now_ms;
    if !upsert_batch.is_empty() {
        commit_ts = state
            .vector_workspace_store
            .upsert_records(workspace_id, &upsert_batch)
            .await?;
    }
    if !insert_batch.is_empty() {
        commit_ts = state
            .vector_workspace_store
            .insert_records(workspace_id, &insert_batch)
            .await?;
    }

    timer.success();
    Ok(UpsertResponse { ids, commit_ts })
}

/// Resolved input ready to be paired with an embedding.
struct NormalizedRecord {
    pk: String,
    id: String,
    /// Whether the caller supplied `id` (vs a server-generated UUID). Drives
    /// write routing: id-less records always take the insert fast path.
    had_explicit_id: bool,
    dataset: String,
    category: String,
    tags: Vec<String>,
    text: String,
    meta: serde_json::Value,
}

fn normalize_record(rec: &NewRecord, dataset: &str) -> Result<NormalizedRecord, AppError> {
    validate::validate_text(&rec.text)?;
    let had_explicit_id = rec.id.is_some();
    let id = match rec.id.as_deref() {
        Some(rk) => {
            validate::validate_id(rk)?;
            rk.to_string()
        }
        // No id → server-generated UUID. Documented as insert-only
        // semantics (no upsert dedup); caller must pass id for upsert.
        None => Uuid::new_v4().to_string().replace('-', ""),
    };
    let pk = validate::build_pk(dataset, &id)?;
    let category = rec
        .category
        .clone()
        .unwrap_or_else(|| validate::DEFAULT_CATEGORY.to_string());
    validate::validate_category(&category)?;
    let tags = rec.tags.clone().unwrap_or_default();
    validate::validate_tags(&tags)?;
    let meta = rec.meta.clone().unwrap_or_else(|| json!({}));
    validate::validate_meta(&meta)?;
    Ok(NormalizedRecord {
        pk,
        id,
        had_explicit_id,
        dataset: dataset.to_string(),
        category,
        tags,
        text: rec.text.clone(),
        meta,
    })
}

/// Resolve + verify the active dataset for a vectors call. The target
/// workspace now comes from the `wk_` bearer (AuthDbWorkspace), so only the
/// dataset is resolved here.
///
/// **Returns DB-canonical `ds.name`** (not the caller-supplied string).
/// The MySQL collation `utf8mb4_0900_ai_ci` is case-insensitive, so a
/// lookup by `"Default"` matches the bootstrap `"default"` row — but the
/// caller's verbatim case would leak into Milvus, creating split state:
/// MySQL holds `"default"`, Milvus rows say `"Default"`, and search by
/// `"default"` (the implicit fallback) would miss those rows entirely.
/// Returning `ds.name` propagates the canonical case forward.
async fn resolve_dataset(
    state: &AppState,
    workspace_id: &str,
    body_dataset: Option<&str>,
) -> Result<String, AppError> {
    let dataset_name = body_dataset
        .unwrap_or(validate::DEFAULT_DATASET)
        .to_string();
    validate::validate_dataset_name(&dataset_name)?;
    let ds = state
        .auth_store
        .get_active_dataset_by_name(workspace_id, &dataset_name)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("dataset {dataset_name}")))?;
    Ok(ds.name)
}

async fn search_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorSearchRequest>,
) -> Result<Json<ApiResponse<VectorSearchResponse>>, AppError> {
    let resp = do_search(&state, &auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Core search: validate → embed → store search → relevance floor. Shared by
/// the `wk_` data plane (above) and the platform gateway (`project_data.rs`).
pub(crate) async fn do_search(
    state: &AppState,
    workspace_id: &str,
    req: VectorSearchRequest,
) -> Result<VectorSearchResponse, AppError> {
    let mut timer = VectorReqTimer::start(
        "search",
        workspace_id,
        req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        req.mode.map(search_mode_label).unwrap_or("hybrid"),
    );
    validate::validate_text(&req.query)?;
    // Validate projection before the (paid) embedding call — fail fast on a
    // bad output_fields instead of spending an embed we'll reject.
    if let Some(fields) = req.output_fields.as_deref() {
        validate::validate_output_fields(fields)?;
    }
    let top_k = match req.top_k {
        None => DEFAULT_TOP_K,
        Some(0) => {
            return Err(VedaError::InvalidInput("top_k must be > 0".into()).into());
        }
        Some(n) if n > MAX_TOP_K => {
            return Err(VedaError::PayloadTooLarge(format!(
                "top_k {n} exceeds {MAX_TOP_K}"
            ))
            .into());
        }
        Some(n) => n,
    };

    let dataset_name =
        resolve_dataset(state, workspace_id, req.dataset.as_deref()).await?;
    timer.set_dataset(&dataset_name);

    // Default is explicit (NOT `unwrap_or_default()`): even though
    // SearchMode::default() also happens to be Hybrid, db's default must not
    // silently couple to the fs enum's default — they're independent contracts.
    // Hybrid gives the best out-of-box recall (dense + BM25 fused); callers
    // opt into `semantic` / `fulltext` explicitly.
    let mode = req.mode.unwrap_or(SearchMode::Hybrid);

    // min_score is a relevance floor; validate before the (paid) embed so we
    // fail fast. It only applies where `score` is an interpretable similarity
    // (semantic=cosine, fulltext=bm25). On hybrid the score is an RRF rank
    // artifact, not relevance — reject rather than silently apply a
    // meaningless threshold. Default mode is hybrid, so a caller must
    // explicitly pick semantic/fulltext to use min_score.
    if let Some(ms) = req.min_score {
        if !ms.is_finite() {
            return Err(VedaError::InvalidInput("min_score must be a finite number".into()).into());
        }
        if mode == SearchMode::Hybrid {
            return Err(VedaError::InvalidInput(
                "min_score is only supported for mode=semantic or fulltext; hybrid ranks by RRF \
                 (not a relevance score) — use top_k, or set mode=semantic for a relevance gate"
                    .into(),
            )
            .into());
        }
    }

    // Embed only for modes that need a dense vector. Fulltext (BM25) skips the
    // paid embed entirely — mirrors the fs `search_full` template.
    let query_vector: Option<Vec<f32>> = if matches!(mode, SearchMode::Semantic | SearchMode::Hybrid)
    {
        let vectors = state.vector_embedding.embed(&[req.query.clone()]).await?;
        Some(vectors.into_iter().next().ok_or_else(|| {
            VedaError::EmbeddingFailed("embedded 0 vectors for query".into())
        })?)
    } else {
        None
    };

    // Parse caller's Filter DSL (Stage 4.4) into a Milvus expr string.
    // None → no extra filter; trait merges with base on its own.
    let extra_filter = match req.filter.as_ref() {
        Some(f) => crate::filter::to_milvus_expr(f)?,
        None => None,
    };

    let search_query = match mode {
        SearchMode::Semantic => VectorSearchQuery::Semantic {
            vector: query_vector.as_deref().expect("semantic embeds above"),
        },
        SearchMode::Hybrid => VectorSearchQuery::Hybrid {
            vector: query_vector.as_deref().expect("hybrid embeds above"),
            text: &req.query,
        },
        SearchMode::Fulltext => VectorSearchQuery::Fulltext { text: &req.query },
    };

    let mut hits = state
        .vector_workspace_store
        .search_vectors(
            workspace_id,
            &dataset_name,
            search_query,
            top_k,
            extra_filter.as_deref(),
            req.output_fields.as_deref(),
        )
        .await?;
    // Relevance floor (semantic/fulltext only — hybrid already rejected above).
    // Post-filter the top_k set, so the response may carry fewer than top_k.
    if let Some(ms) = req.min_score {
        hits.retain(|h| h.score >= ms);
    }
    timer.success();
    Ok(VectorSearchResponse { hits })
}

/// Cap on `ids` for query/delete. Matches MAX_RECORDS_PER_UPSERT so
/// batch shape is symmetric across endpoints; oversize → 413.
const MAX_PK_BATCH: usize = 500;

async fn query_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorQueryRequest>,
) -> Result<Json<ApiResponse<VectorQueryResponse>>, AppError> {
    let resp = do_query(&state, &auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Core query-by-id. Shared by the `wk_` data plane and the platform gateway.
pub(crate) async fn do_query(
    state: &AppState,
    workspace_id: &str,
    req: VectorQueryRequest,
) -> Result<VectorQueryResponse, AppError> {
    let mut timer = VectorReqTimer::start(
        "query",
        workspace_id,
        req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        "none",
    );
    if req.ids.is_empty() {
        return Err(VedaError::InvalidInput("ids must not be empty".into()).into());
    }
    if req.ids.len() > MAX_PK_BATCH {
        return Err(VedaError::PayloadTooLarge(format!(
            "ids: {} exceeds {MAX_PK_BATCH}",
            req.ids.len()
        ))
        .into());
    }
    let dataset_name =
        resolve_dataset(state, workspace_id, req.dataset.as_deref()).await?;
    timer.set_dataset(&dataset_name);
    if let Some(fields) = req.output_fields.as_deref() {
        validate::validate_output_fields(fields)?;
    }
    let pks = build_pks(&dataset_name, &req.ids)?;
    let hits = state
        .vector_workspace_store
        .query_vectors_by_pk(workspace_id, &pks, req.output_fields.as_deref())
        .await?;
    timer.success();
    Ok(VectorQueryResponse { hits })
}

async fn delete_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorDeleteRequest>,
) -> Result<Json<ApiResponse<VectorDeleteResponse>>, AppError> {
    auth.require_write()?;
    let resp = do_delete(&state, &auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

/// Core delete-by-id. Shared by the `wk_` data plane and the platform gateway.
pub(crate) async fn do_delete(
    state: &AppState,
    workspace_id: &str,
    req: VectorDeleteRequest,
) -> Result<VectorDeleteResponse, AppError> {
    let mut timer = VectorReqTimer::start(
        "delete",
        workspace_id,
        req.dataset.as_deref().unwrap_or(validate::DEFAULT_DATASET),
        "none",
    );
    if req.ids.is_empty() {
        return Err(VedaError::InvalidInput("ids must not be empty".into()).into());
    }
    if req.ids.len() > MAX_PK_BATCH {
        return Err(VedaError::PayloadTooLarge(format!(
            "ids: {} exceeds {MAX_PK_BATCH}",
            req.ids.len()
        ))
        .into());
    }
    let dataset_name =
        resolve_dataset(state, workspace_id, req.dataset.as_deref()).await?;
    timer.set_dataset(&dataset_name);
    let pks = build_pks(&dataset_name, &req.ids)?;
    let delete_count = state
        .vector_workspace_store
        .delete_vectors_by_pk(workspace_id, &pks)
        .await?;
    timer.success();
    Ok(VectorDeleteResponse { delete_count })
}

fn build_pks(dataset: &str, ids: &[String]) -> Result<Vec<String>, AppError> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(validate::build_pk(dataset, id)?);
    }
    Ok(out)
}

/// RAII timer for the end-to-end vector request histogram
/// `veda_vector_request_seconds` (includes embedding + store + assembly — the
/// user-perceived latency). Drops with `outcome=err` unless `success()` is
/// called, so `?` early returns are recorded as errors automatically. Labels:
/// operation, workspace_id, dataset (request value), mode (search only), outcome.
struct VectorReqTimer {
    operation: &'static str,
    workspace_id: String,
    dataset: String,
    mode: &'static str,
    started: std::time::Instant,
    ok: bool,
}

impl VectorReqTimer {
    fn start(operation: &'static str, workspace_id: &str, dataset: &str, mode: &'static str) -> Self {
        Self {
            operation,
            workspace_id: workspace_id.to_string(),
            // Raw request dataset, truncated to bound label cardinality against
            // a malicious/oversized input. Replaced with the DB-canonical name
            // via set_dataset once resolve_dataset succeeds.
            dataset: dataset.chars().take(64).collect(),
            mode,
            started: std::time::Instant::now(),
            ok: false,
        }
    }

    /// Replace the dataset label with the DB-canonical name (post-resolve) so
    /// the end-to-end layer matches the store layer's dataset for three-layer
    /// comparison (avoids e.g. "Default" vs canonical "default" splitting).
    fn set_dataset(&mut self, dataset: &str) {
        self.dataset = dataset.to_string();
    }

    fn success(&mut self) {
        self.ok = true;
    }
}

impl Drop for VectorReqTimer {
    fn drop(&mut self) {
        ::metrics::histogram!(
            "veda_vector_request_seconds",
            "operation" => self.operation,
            "workspace_id" => self.workspace_id.clone(),
            "dataset" => self.dataset.clone(),
            "mode" => self.mode,
            "outcome" => if self.ok { "ok" } else { "err" },
        )
        .record(self.started.elapsed().as_secs_f64());
    }
}

fn search_mode_label(m: SearchMode) -> &'static str {
    match m {
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
        SearchMode::Fulltext => "fulltext",
    }
}

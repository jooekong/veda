//! Pinecone-style vectors data plane (db-kind workspaces only).
//!
//! Stage 4.2 lands `POST /v1/vectors/upsert` only. Stage 4.3 will add
//! search / query / delete in the same module.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use veda_types::api::{
    NewRecord, UpsertRequest, UpsertResponse, VectorDeleteRequest, VectorDeleteResponse,
    VectorQueryRequest, VectorQueryResponse, VectorSearchRequest, VectorSearchResponse,
};
use veda_types::{validate, ApiResponse, UpsertRecord, VedaError, Workspace};

use crate::auth::AuthAccount;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/vectors/upsert", post(upsert_vectors))
        .route("/v1/vectors/search", post(search_vectors))
        .route("/v1/vectors/query", post(query_vectors))
        .route("/v1/vectors/delete", post(delete_vectors))
}

/// top_k default + ceiling from plan §3.2 / vss design.
const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;

/// Batch cap matches plan §3.2 limit; oversized batches return 413 (via
/// `VedaError::PayloadTooLarge`) instead of letting Milvus / embedding
/// upstream reject opaquely. Matches vss `openapi.yaml` Error413 contract.
const MAX_RECORDS_PER_UPSERT: usize = 500;

async fn upsert_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<ApiResponse<UpsertResponse>>, AppError> {
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

    // 1. Resolve workspace_id.
    let ws_id = resolve_workspace_id(&auth, req.workspace_id.as_deref())?;

    // 2. load_db_workspace: ownership + kind=Db + token scope.
    let ws: Workspace = auth.load_db_workspace(&state, &ws_id).await?;

    // 3. Resolve dataset (body or implicit default), then verify the
    //    veda_datasets row exists and is active. Without this check, an
    //    upsert to a typo'd dataset would silently land in Milvus with no
    //    corresponding control-plane row.
    let dataset_name = req
        .dataset
        .as_deref()
        .unwrap_or(validate::DEFAULT_DATASET)
        .to_string();
    validate::validate_dataset_name(&dataset_name)?;
    // Canonicalize from DB — see resolve_db_target rationale (case-insensitive
    // MySQL collation vs case-preserving Milvus rows would split state).
    let ds = state
        .auth_store
        .get_active_dataset_by_name(&ws.id, &dataset_name)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("dataset {dataset_name}")))?;
    let dataset_name = ds.name;

    // 4. Validate every record and resolve defaults BEFORE the embedding
    //    call — embedding is the expensive step, no point spending it on
    //    a batch we'll reject later. Each record is normalized into
    //    `(id, NormalizedFields)` plus the text to embed.
    let now_ms = Utc::now().timestamp_millis();
    let mut texts: Vec<String> = Vec::with_capacity(req.records.len());
    let mut normalized: Vec<NormalizedRecord> = Vec::with_capacity(req.records.len());
    for rec in &req.records {
        let nr = normalize_record(rec, &dataset_name)?;
        texts.push(nr.text.clone());
        normalized.push(nr);
    }

    // 5. Batched embed — one upstream call (cache hits skip).
    let vectors = state.vector_embedding.embed(&texts).await?;
    if vectors.len() != normalized.len() {
        // EmbeddingCache + EmbeddingProvider both guarantee 1-to-1; treat
        // any mismatch as an upstream contract violation, not silent loss.
        return Err(VedaError::EmbeddingFailed(format!(
            "embedded {} vectors for {} records",
            vectors.len(),
            normalized.len()
        ))
        .into());
    }

    // 6. Build Milvus payload records.
    let mut to_insert: Vec<UpsertRecord> = Vec::with_capacity(normalized.len());
    let mut ids: Vec<String> = Vec::with_capacity(normalized.len());
    for (nr, vec) in normalized.into_iter().zip(vectors.into_iter()) {
        ids.push(nr.id.clone());
        to_insert.push(UpsertRecord {
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
        });
    }

    // 7. Synchronous upsert. commit_ts is server-now (Milvus REST doesn't
    //    surface a real one; see VectorWorkspaceStore::upsert_records doc).
    //
    // Same-batch duplicate `id`: Milvus PK upsert semantics — last entry
    // wins. v0 doesn't dedupe server-side; the response `ids` echoes the
    // request order. TODO(#7): explicit integration test for this once the
    // idempotency-docs task lands.
    let commit_ts = state
        .vector_workspace_store
        .upsert_records(&ws.id, &to_insert)
        .await?;

    Ok(Json(ApiResponse::ok(UpsertResponse { ids, commit_ts })))
}

/// Resolved input ready to be paired with an embedding.
struct NormalizedRecord {
    pk: String,
    id: String,
    dataset: String,
    category: String,
    tags: Vec<String>,
    text: String,
    meta: serde_json::Value,
}

fn normalize_record(rec: &NewRecord, dataset: &str) -> Result<NormalizedRecord, AppError> {
    validate::validate_text(&rec.text)?;
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
        .unwrap_or_else(|| validate::DEFAULT_DATASET.to_string());
    validate::validate_category(&category)?;
    let tags = rec.tags.clone().unwrap_or_default();
    validate::validate_tags(&tags)?;
    let meta = rec.meta.clone().unwrap_or_else(|| json!({}));
    validate::validate_meta(&meta)?;
    Ok(NormalizedRecord {
        pk,
        id,
        dataset: dataset.to_string(),
        category,
        tags,
        text: rec.text.clone(),
        meta,
    })
}

/// Resolves the target workspace_id from the body (explicit) or the
/// token's `allowed_workspaces`.
///
/// **Implicit default is only allowed when the token's scope is exactly one
/// workspace** — that's the single case where there's no ambiguity. With
/// `allowed_workspaces = [a, b]` and no body field, silently picking `a`
/// is a footgun (caller may have meant `b`); reject with 400 instead and
/// force the caller to specify.
fn resolve_workspace_id(auth: &AuthAccount, from_body: Option<&str>) -> Result<String, AppError> {
    if let Some(s) = from_body {
        return Ok(s.to_string());
    }
    match auth.allowed_workspaces.as_deref() {
        Some([only]) => Ok(only.clone()),
        Some(many) if many.len() > 1 => Err(VedaError::InvalidInput(format!(
            "workspace_id required: token has {} allowed_workspaces, omitted body field is ambiguous",
            many.len()
        ))
        .into()),
        // Empty list (deny-all) or unrestricted (None): no implicit default available.
        _ => Err(VedaError::InvalidInput(
            "workspace_id required when token has no single-workspace default".into(),
        )
        .into()),
    }
}

/// Common preamble for all data-plane handlers: resolve workspace_id,
/// load+authorize the db-kind workspace, resolve dataset (body or
/// implicit default), verify it's active.
///
/// **Returns DB-canonical `ds.name`** (not the caller-supplied string).
/// The MySQL collation `utf8mb4_0900_ai_ci` is case-insensitive, so a
/// lookup by `"Default"` matches the bootstrap `"default"` row — but the
/// caller's verbatim case would leak into Milvus, creating split state:
/// MySQL holds `"default"`, Milvus rows say `"Default"`, and search by
/// `"default"` (the implicit fallback) would miss those rows entirely.
/// Returning `ds.name` propagates the canonical case forward.
async fn resolve_db_target(
    state: &AppState,
    auth: &AuthAccount,
    body_workspace_id: Option<&str>,
    body_dataset: Option<&str>,
) -> Result<(Workspace, String), AppError> {
    let ws_id = resolve_workspace_id(auth, body_workspace_id)?;
    let ws = auth.load_db_workspace(state, &ws_id).await?;
    let dataset_name = body_dataset
        .unwrap_or(validate::DEFAULT_DATASET)
        .to_string();
    validate::validate_dataset_name(&dataset_name)?;
    let ds = state
        .auth_store
        .get_active_dataset_by_name(&ws.id, &dataset_name)
        .await?
        .ok_or_else(|| VedaError::NotFound(format!("dataset {dataset_name}")))?;
    Ok((ws, ds.name))
}

async fn search_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<VectorSearchRequest>,
) -> Result<Json<ApiResponse<VectorSearchResponse>>, AppError> {
    validate::validate_text(&req.query)?;
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

    let (ws, dataset_name) = resolve_db_target(
        &state,
        &auth,
        req.workspace_id.as_deref(),
        req.dataset.as_deref(),
    )
    .await?;

    // Embed the query (single text — batched API but one item is fine).
    let vectors = state.vector_embedding.embed(&[req.query.clone()]).await?;
    let query_vector = vectors.into_iter().next().ok_or_else(|| {
        VedaError::EmbeddingFailed("embedded 0 vectors for query".into())
    })?;

    // Parse caller's Filter DSL (Stage 4.4) into a Milvus expr string.
    // None → no extra filter; trait merges with base on its own.
    let extra_filter = match req.filter.as_ref() {
        Some(f) => crate::filter::to_milvus_expr(f)?,
        None => None,
    };

    let hits = state
        .vector_workspace_store
        .search_vectors(
            &ws.id,
            &dataset_name,
            &query_vector,
            top_k,
            extra_filter.as_deref(),
        )
        .await?;
    Ok(Json(ApiResponse::ok(VectorSearchResponse { hits })))
}

/// Cap on `ids` for query/delete. Matches MAX_RECORDS_PER_UPSERT so
/// batch shape is symmetric across endpoints; oversize → 413.
const MAX_PK_BATCH: usize = 500;

async fn query_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<VectorQueryRequest>,
) -> Result<Json<ApiResponse<VectorQueryResponse>>, AppError> {
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
    let (ws, dataset_name) = resolve_db_target(
        &state,
        &auth,
        req.workspace_id.as_deref(),
        req.dataset.as_deref(),
    )
    .await?;
    let pks = build_pks(&dataset_name, &req.ids)?;
    let hits = state
        .vector_workspace_store
        .query_vectors_by_pk(&ws.id, &pks)
        .await?;
    Ok(Json(ApiResponse::ok(VectorQueryResponse { hits })))
}

async fn delete_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Json(req): Json<VectorDeleteRequest>,
) -> Result<Json<ApiResponse<VectorDeleteResponse>>, AppError> {
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
    let (ws, dataset_name) = resolve_db_target(
        &state,
        &auth,
        req.workspace_id.as_deref(),
        req.dataset.as_deref(),
    )
    .await?;
    let pks = build_pks(&dataset_name, &req.ids)?;
    let accepted_count = state
        .vector_workspace_store
        .delete_vectors_by_pk(&ws.id, &pks)
        .await?;
    Ok(Json(ApiResponse::ok(VectorDeleteResponse {
        accepted_count,
    })))
}

fn build_pks(dataset: &str, ids: &[String]) -> Result<Vec<String>, AppError> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(validate::build_pk(dataset, id)?);
    }
    Ok(out)
}

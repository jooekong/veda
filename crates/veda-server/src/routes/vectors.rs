//! Pinecone-style vectors data plane (db-kind workspaces only).
//!
//! Thin HTTP shell: auth + JSON in/out. All business logic (validation,
//! dedupe, embedding, store writes) lives in
//! `veda_core::service::vector::VectorService`, shared with the platform
//! gateway surface (`project_data.rs`).

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, Router};
use veda_types::api::{
    UpsertRequest, UpsertResponse, VectorDeleteRequest, VectorDeleteResponse, VectorQueryRequest,
    VectorQueryResponse, VectorSearchRequest, VectorSearchResponse,
};
use veda_types::ApiResponse;

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
        // bare error before the service's structured PayloadTooLarge check.
        .layer(DefaultBodyLimit::max(MAX_BODY_MB * 1024 * 1024))
}

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
    let resp = state.vector_service.upsert(&auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn search_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorSearchRequest>,
) -> Result<Json<ApiResponse<VectorSearchResponse>>, AppError> {
    let resp = state.vector_service.search(&auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn query_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorQueryRequest>,
) -> Result<Json<ApiResponse<VectorQueryResponse>>, AppError> {
    let resp = state.vector_service.query(&auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

async fn delete_vectors(
    State(state): State<Arc<AppState>>,
    auth: AuthDbWorkspace,
    Json(req): Json<VectorDeleteRequest>,
) -> Result<Json<ApiResponse<VectorDeleteResponse>>, AppError> {
    auth.require_write()?;
    let resp = state.vector_service.delete(&auth.workspace_id, req).await?;
    Ok(Json(ApiResponse::ok(resp)))
}

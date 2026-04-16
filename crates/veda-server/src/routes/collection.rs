use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use veda_types::api::{CollectionSearchRequest, CreateCollectionRequest, InsertRowsRequest};
use veda_types::{ApiResponse, CollectionSchema, CollectionType, VedaError};

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/collections", post(create_collection).get(list_collections))
        .route("/v1/collections/{name}", get(describe_collection).delete(delete_collection))
        .route("/v1/collections/{name}/rows", post(insert_rows))
        .route("/v1/collections/{name}/search", post(search_collection))
}

async fn create_collection(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<CreateCollectionRequest>,
) -> Result<Json<ApiResponse<CollectionSchema>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let ct = req.collection_type.unwrap_or(CollectionType::Structured);
    let schema = state
        .collection_service
        .create(&auth.workspace_id, &req.name, ct, &req.fields)
        .await?;
    Ok(Json(ApiResponse::ok(schema)))
}

async fn list_collections(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
) -> Result<Json<ApiResponse<Vec<CollectionSchema>>>, AppError> {
    let list = state.collection_service.list(&auth.workspace_id).await?;
    Ok(Json(ApiResponse::ok(list)))
}

async fn describe_collection(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(name): Path<String>,
) -> Result<Json<ApiResponse<CollectionSchema>>, AppError> {
    let schema = state.collection_service.get(&auth.workspace_id, &name).await?;
    Ok(Json(ApiResponse::ok(schema)))
}

async fn delete_collection(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(name): Path<String>,
) -> Result<Json<ApiResponse<()>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    state
        .collection_service
        .delete(&auth.workspace_id, &name)
        .await?;
    Ok(Json(ApiResponse::ok(())))
}

async fn insert_rows(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(name): Path<String>,
    Json(req): Json<InsertRowsRequest>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    if auth.read_only {
        return Err(VedaError::PermissionDenied.into());
    }
    let count = state
        .collection_service
        .insert_rows(&auth.workspace_id, &name, &req.rows)
        .await?;
    Ok(Json(ApiResponse::ok(serde_json::json!({ "inserted": count }))))
}

async fn search_collection(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(name): Path<String>,
    Json(req): Json<CollectionSearchRequest>,
) -> Result<Json<ApiResponse<Vec<serde_json::Value>>>, AppError> {
    let limit = req.limit.unwrap_or(10);
    let results = state
        .collection_service
        .search(&auth.workspace_id, &name, &req.query, limit)
        .await?;
    Ok(Json(ApiResponse::ok(results)))
}

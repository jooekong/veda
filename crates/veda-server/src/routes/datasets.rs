use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;
use veda_types::{validate, ApiResponse, Dataset, DatasetStatus, VedaError};

use crate::auth::AuthAccount;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/workspaces/{ws}/datasets",
            post(create_dataset).get(list_datasets),
        )
        .route(
            "/v1/workspaces/{ws}/datasets/{name}",
            delete(delete_dataset),
        )
}

#[derive(Debug, Deserialize)]
struct CreateDatasetRequest {
    name: String,
}

async fn create_dataset(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(ws_id): Path<String>,
    Json(req): Json<CreateDatasetRequest>,
) -> Result<(StatusCode, Json<ApiResponse<Dataset>>), AppError> {
    // load_db_workspace bundles ownership + kind=Db + token scope checks.
    let ws = auth.load_db_workspace(&state, &ws_id).await?;
    validate::validate_dataset_name(&req.name)?;

    let now = Utc::now();
    let dataset = Dataset {
        id: Uuid::new_v4().to_string(),
        workspace_id: ws.id,
        name: req.name,
        status: DatasetStatus::Active,
        created_at: now,
        updated_at: now,
    };
    // mysql create_dataset maps UNIQUE conflict (1062) to AlreadyExists,
    // which the AppError IntoResponse maps to 409.
    state.auth_store.create_dataset(&dataset).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(dataset))))
}

async fn list_datasets(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path(ws_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<Dataset>>>, AppError> {
    let ws = auth.load_db_workspace(&state, &ws_id).await?;
    let datasets = state.auth_store.list_active_datasets(&ws.id).await?;
    Ok(Json(ApiResponse::ok(datasets)))
}

async fn delete_dataset(
    State(state): State<Arc<AppState>>,
    auth: AuthAccount,
    Path((ws_id, name)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    // Auth + scope FIRST. Don't leak business-rule signals (e.g. "default
    // is the reserved name") to unauthenticated callers — they should get
    // a generic auth failure regardless of which name they try.
    let ws = auth.load_db_workspace(&state, &ws_id).await?;

    // The bootstrap dataset is the implicit fallback for vector API calls
    // that omit the `dataset` field. Deleting it would break every such
    // caller silently — refuse.
    if name == validate::DEFAULT_DATASET {
        return Err(VedaError::CannotDeleteDefaultDataset.into());
    }
    validate::validate_dataset_name(&name)?;

    let archived = state.auth_store.archive_dataset(&ws.id, &name).await?;
    if !archived {
        return Err(VedaError::NotFound(format!("dataset {name}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

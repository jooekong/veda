use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use veda_types::api::SqlRequest;
use veda_types::ApiResponse;

use crate::auth::AuthWorkspace;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/sql", post(execute_sql))
}

async fn execute_sql(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Json(req): Json<SqlRequest>,
) -> std::result::Result<Json<ApiResponse<Vec<serde_json::Value>>>, AppError> {
    let batches = state
        .sql_engine
        .execute(&auth.workspace_id, auth.read_only, &req.sql)
        .await?;

    let buf = Vec::new();
    let mut writer = arrow::json::ArrayWriter::new(buf);
    for batch in &batches {
        writer
            .write(batch)
            .map_err(|e| AppError(veda_types::VedaError::Storage(e.to_string())))?;
    }
    writer
        .finish()
        .map_err(|e| AppError(veda_types::VedaError::Storage(e.to_string())))?;
    let bytes = writer.into_inner();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|e| AppError(veda_types::VedaError::Storage(format!("arrow json parse failed: {e}"))))?;

    Ok(Json(ApiResponse::ok(rows)))
}

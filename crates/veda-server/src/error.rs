use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use tracing::error;
use veda_types::{ApiResponse, VedaError};

pub struct AppError(pub VedaError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self.0 {
            VedaError::NotFound(p) => (StatusCode::NOT_FOUND, format!("not found: {p}")),
            VedaError::AlreadyExists(p) => (StatusCode::CONFLICT, format!("already exists: {p}")),
            VedaError::PermissionDenied => (StatusCode::FORBIDDEN, "permission denied".to_string()),
            VedaError::InvalidPath(p) => (StatusCode::BAD_REQUEST, format!("invalid path: {p}")),
            VedaError::InvalidInput(p) => (StatusCode::BAD_REQUEST, format!("invalid input: {p}")),
            VedaError::QuotaExceeded(p) => (StatusCode::TOO_MANY_REQUESTS, format!("quota exceeded: {p}")),
            _ => {
                error!(err = %self.0, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
            }
        };
        (status, Json(ApiResponse::<()>::err(msg))).into_response()
    }
}

impl From<VedaError> for AppError {
    fn from(e: VedaError) -> Self {
        AppError(e)
    }
}

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use veda_types::{ApiResponse, VedaError};

pub struct AppError(pub VedaError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            VedaError::NotFound(_) => StatusCode::NOT_FOUND,
            VedaError::AlreadyExists(_) => StatusCode::CONFLICT,
            VedaError::PermissionDenied => StatusCode::FORBIDDEN,
            VedaError::InvalidPath(_) | VedaError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            VedaError::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(ApiResponse::<()>::err(self.0.to_string()))).into_response()
    }
}

impl From<VedaError> for AppError {
    fn from(e: VedaError) -> Self {
        AppError(e)
    }
}

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use tracing::error;
use veda_types::{ApiResponse, VedaError};

#[derive(Debug)]
pub struct AppError(pub VedaError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            VedaError::NotFound(_) => StatusCode::NOT_FOUND,
            VedaError::AlreadyExists(_) => StatusCode::CONFLICT,
            VedaError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            VedaError::PermissionDenied => StatusCode::FORBIDDEN,
            VedaError::WorkspaceKindMismatch | VedaError::CannotDeleteDefaultDataset => {
                StatusCode::BAD_REQUEST
            }
            VedaError::InvalidPath(_) | VedaError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            VedaError::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            VedaError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            VedaError::PreconditionFailed(_) => StatusCode::PRECONDITION_FAILED,
            VedaError::EmbeddingFailed(_)
            | VedaError::Deadlock(_)
            | VedaError::Storage(_)
            | VedaError::Internal(_) => {
                // Log server-internal errors with full detail. The wire
                // response only carries the opaque `INTERNAL` code (set
                // by VedaError::code()) so storage / backend specifics
                // never leak to callers.
                error!(err = %self.0, "internal error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let body = ApiResponse::<()>::from_veda_error(&self.0);
        (status, Json(body)).into_response()
    }
}

impl From<VedaError> for AppError {
    fn from(e: VedaError) -> Self {
        AppError(e)
    }
}

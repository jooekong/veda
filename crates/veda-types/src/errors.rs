use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum VedaError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("permission denied")]
    PermissionDenied,

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    #[error("embedding failed: {0}")]
    EmbeddingFailed(String),

    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    #[error("deadlock: {0}")]
    Deadlock(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, VedaError>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiResponse<T: serde::Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: serde::Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }
}

impl ApiResponse<()> {
    pub fn err(msg: impl fmt::Display) -> ApiResponse<()> {
        ApiResponse {
            success: false,
            data: None,
            error: Some(msg.to_string()),
        }
    }
}


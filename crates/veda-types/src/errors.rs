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

    /// Caller hit an endpoint scoped to the wrong workspace kind
    /// (e.g. fs API on a db workspace, or vectors API on an fs workspace).
    /// Distinct from generic `InvalidInput` so business apps can match on
    /// `error_code` instead of parsing a free-form message string.
    #[error("workspace kind does not match this API path")]
    WorkspaceKindMismatch,

    /// Caller tried to `DELETE /v1/workspaces/{ws}/datasets/default`. The
    /// implicit-fallback dataset is reserved; archiving it would silently
    /// break every vector API call that omits `dataset`.
    #[error("cannot delete the default dataset")]
    CannotDeleteDefaultDataset,

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

impl VedaError {
    /// Stable, machine-readable code surfaced in `ApiResponse::error_code`.
    /// Business apps should match on these strings instead of parsing the
    /// human-readable `error` message (whose wording may change).
    ///
    /// Server-internal variants (Storage / Deadlock / Internal) collapse
    /// to `INTERNAL` — never leak storage-backend specifics to callers.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "NOT_FOUND",
            Self::AlreadyExists(_) => "ALREADY_EXISTS",
            Self::Unauthorized(_) => "UNAUTHORIZED",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::WorkspaceKindMismatch => "WORKSPACE_KIND_MISMATCH",
            Self::CannotDeleteDefaultDataset => "CANNOT_DELETE_DEFAULT_DATASET",
            Self::InvalidPath(_) => "INVALID_PATH",
            Self::InvalidInput(_) => "INVALID_INPUT",
            Self::QuotaExceeded(_) => "QUOTA_EXCEEDED",
            Self::PayloadTooLarge(_) => "PAYLOAD_TOO_LARGE",
            Self::EmbeddingFailed(_) => "EMBEDDING_FAILED",
            Self::PreconditionFailed(_) => "PRECONDITION_FAILED",
            Self::Deadlock(_) | Self::Storage(_) | Self::Internal(_) => "INTERNAL",
        }
    }
}

pub type Result<T> = std::result::Result<T, VedaError>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiResponse<T: serde::Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// Stable machine-readable code (e.g. `NOT_FOUND`, `INVALID_INPUT`).
    /// Always present on error responses; never on success. Business apps
    /// should match on this instead of `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    /// Human-readable description. Wording may evolve; do not match on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<T: serde::Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error_code: None,
            error: None,
        }
    }
}

impl ApiResponse<()> {
    /// Build an error response with both a machine-readable code and a
    /// human-readable message. Prefer `from_veda_error` when the error
    /// originates from `VedaError` — it derives both fields automatically
    /// from the variant.
    pub fn err(code: &'static str, msg: impl fmt::Display) -> ApiResponse<()> {
        ApiResponse {
            success: false,
            data: None,
            error_code: Some(code),
            error: Some(msg.to_string()),
        }
    }

    pub fn from_veda_error(e: &VedaError) -> ApiResponse<()> {
        Self::err(e.code(), e.to_string())
    }
}

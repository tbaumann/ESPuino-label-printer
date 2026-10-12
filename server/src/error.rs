use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;

/// Unified API error. Rendered as `{"error": "..."}` with an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A feature this deployment has not been configured for. The message says
    /// which: the admin API wants `ADMIN_TOKEN`, the shop wants
    /// `STRIPE_SECRET_KEY`. Deliberately generic, because both use it and
    /// "admin API unavailable" was a lie the moment payments arrived.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// A third party (Stripe) was unreachable or answered with an error. Kept
    /// separate from `Internal` so the two are distinguishable in logs and in
    /// the HTTP status: 502 says "not our fault, retryable".
    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Upstream(_) => StatusCode::BAD_GATEWAY,
            ApiError::Db(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            ApiError::Db(_) | ApiError::Io(_) | ApiError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        let body = Json(json!({ "error": self.to_string() }));
        (status, body).into_response()
    }
}

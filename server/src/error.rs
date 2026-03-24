use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("config: {0}")]
    Config(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("approval not found: {0}")]
    ApprovalNotFound(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            AppError::ApprovalNotFound(_) => StatusCode::NOT_FOUND,
            AppError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

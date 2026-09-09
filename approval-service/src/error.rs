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

    #[error("approval request conflict: {0}")]
    ApprovalConflict(String),

    #[error("approval request exceeds the canonical payload limit")]
    ApprovalRequestTooLarge,

    #[error("approval has reached the active waiter limit")]
    ApprovalWaitersFull,

    #[error("invalid approval waiter ID")]
    InvalidApprovalWaiter,

    #[error("invalid approval request: {0}")]
    InvalidApprovalRequest(String),

    #[error("approval resolution is disabled in read-only mode")]
    ApprovalReadonly,

    #[error("question not found: {0}")]
    QuestionNotFound(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            AppError::ApprovalNotFound(_) => StatusCode::NOT_FOUND,
            AppError::ApprovalConflict(_) => StatusCode::CONFLICT,
            AppError::ApprovalRequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::ApprovalWaitersFull => StatusCode::TOO_MANY_REQUESTS,
            AppError::InvalidApprovalWaiter => StatusCode::BAD_REQUEST,
            AppError::InvalidApprovalRequest(_) => StatusCode::BAD_REQUEST,
            AppError::ApprovalReadonly => StatusCode::FORBIDDEN,
            AppError::QuestionNotFound(_) => StatusCode::NOT_FOUND,
            AppError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

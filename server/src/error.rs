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

    #[error("question not found: {0}")]
    QuestionNotFound(String),

    #[error("workflow not found: {0}")]
    WorkflowNotFound(String),

    #[error("workflow invalid: {0}")]
    WorkflowInvalid(String),

    #[error("work item not found: {0}")]
    WorkItemNotFound(String),

    #[error("job not found: {0}")]
    JobNotFound(String),

    #[error("worker not found: {0}")]
    #[allow(dead_code)]
    WorkerNotFound(String),

    #[error("worker session unauthorized: {0}")]
    WorkerSessionUnauthorized(String),

    #[error("job ownership violation: {0}")]
    #[allow(dead_code)]
    JobOwnership(String),

    #[error("job already cancelled: {0}")]
    JobCancelled(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            AppError::ApprovalNotFound(_) => StatusCode::NOT_FOUND,
            AppError::QuestionNotFound(_) => StatusCode::NOT_FOUND,
            AppError::WorkflowNotFound(_) => StatusCode::NOT_FOUND,
            AppError::WorkItemNotFound(_) => StatusCode::NOT_FOUND,
            AppError::JobNotFound(_) => StatusCode::NOT_FOUND,
            AppError::WorkerNotFound(_) => StatusCode::NOT_FOUND,
            AppError::WorkerSessionUnauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::WorkflowInvalid(_) => StatusCode::BAD_REQUEST,
            AppError::JobOwnership(_) => StatusCode::CONFLICT,
            AppError::JobCancelled(_) => StatusCode::CONFLICT,
            AppError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

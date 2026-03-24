use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use tracing::debug;

use super::AppState;
use super::notifier::Notifier;

/// Auth middleware: validates Bearer token against configured tokens.
pub async fn require_auth<N: Notifier>(
    state: axum::extract::State<AppState<N>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let matched = state
        .config
        .tokens
        .iter()
        .find(|t| t.secret == token)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    debug!(
        label = matched.label,
        path = request.uri().path(),
        "authenticated request"
    );

    Ok(next.run(request).await)
}

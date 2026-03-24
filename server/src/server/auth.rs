use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use tower_sessions::Session;
use tracing::debug;

use super::AppState;
use super::notifier::Notifier;
use super::oauth;

/// API auth middleware: validates Bearer token against configured tokens.
/// Also accepts valid session cookies (for web UI calling API endpoints).
pub async fn require_auth<N: Notifier>(
    state: axum::extract::State<AppState<N>>,
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try Bearer token first
    if let Some(header) = request.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        // If Authorization header is present, it must be valid
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
            "authenticated via bearer token"
        );
        return Ok(next.run(request).await);
    }

    // Try session cookie
    if let Some(email) = oauth::get_session_email(&session).await {
        debug!(
            email,
            path = request.uri().path(),
            "authenticated via session"
        );
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// Web auth middleware: session cookie only, redirects to login on failure.
pub async fn require_web_auth(session: Session, request: Request, next: Next) -> Response {
    if oauth::get_session_email(&session).await.is_some() {
        return next.run(request).await;
    }
    Redirect::temporary("/auth/login").into_response()
}

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use tower_sessions::Session;
use tracing::debug;

use super::AppState;
use super::config::Token;
use super::notifier::Notifier;
use super::oauth;

pub fn match_bearer_token<'a>(header: &str, tokens: &'a [Token]) -> Option<&'a Token> {
    let token = header.strip_prefix("Bearer ")?;
    tokens.iter().find(|t| t.secret.expose() == token)
}

/// API auth middleware: validates Bearer token against configured tokens.
/// Also accepts valid session cookies (for web UI calling API endpoints).
pub async fn require_auth<N: Notifier>(
    state: axum::extract::State<AppState<N>>,
    session: Session,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try Bearer token first
    if let Some(header) = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        // If Authorization header is present, it must be valid
        let matched =
            match_bearer_token(header, &state.config.tokens).ok_or(StatusCode::UNAUTHORIZED)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn token(label: &str, secret: &str) -> Token {
        Token {
            label: label.to_string(),
            secret: protocol::Secret::new(secret),
        }
    }

    #[test]
    fn bearer_token_matches_configured_secret() {
        let tokens = vec![token("desktop", "abc123")];
        let matched = match_bearer_token("Bearer abc123", &tokens).expect("should match");
        assert_eq!(matched.label, "desktop");
    }

    #[test]
    fn bearer_token_rejects_invalid_or_malformed_value() {
        let tokens = vec![token("desktop", "abc123")];
        assert!(match_bearer_token("Bearer wrong", &tokens).is_none());
        assert!(match_bearer_token("abc123", &tokens).is_none());
    }
}

/// Web auth middleware: session cookie only, redirects to login on failure.
pub async fn require_web_auth(session: Session, request: Request, next: Next) -> Response {
    if oauth::get_session_email(&session).await.is_some() {
        return next.run(request).await;
    }
    Redirect::temporary("/auth/login").into_response()
}

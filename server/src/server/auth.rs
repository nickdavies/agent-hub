use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use subtle::ConstantTimeEq;
use tower_sessions::Session;
use tracing::debug;

use super::AppState;
use super::config::Role;
use super::notifier::Notifier;
use super::oauth;

/// The authenticated caller, inserted into request extensions by
/// [`require_auth`] (or as a fixed anonymous reviewer when `AUTH_MODE=none`).
#[derive(Clone, Debug)]
pub struct Principal {
    /// Token label, or email for OIDC/basic sessions.
    pub label: String,
    pub role: Role,
}

impl Principal {
    /// Used when `AUTH_MODE=none`: everyone has full access.
    pub fn anonymous() -> Self {
        Self {
            label: "anonymous".into(),
            role: Role::Reviewer,
        }
    }
}

/// API auth middleware: validates Bearer token against configured tokens.
/// Also accepts valid session cookies (for web UI calling API endpoints),
/// which are always treated as reviewers.
pub async fn require_auth<N: Notifier>(
    state: axum::extract::State<AppState<N>>,
    session: Session,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try Bearer token first
    if let Some(header) = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        // If Authorization header is present, it must be valid
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(StatusCode::UNAUTHORIZED)?;

        let matched = state
            .config
            .tokens
            .iter()
            .find(|t| bool::from(t.secret.expose().as_bytes().ct_eq(token.as_bytes())))
            .ok_or(StatusCode::UNAUTHORIZED)?;

        debug!(
            label = matched.label,
            role = ?matched.role,
            path = request.uri().path(),
            "authenticated via bearer token"
        );
        request.extensions_mut().insert(Principal {
            label: matched.label.clone(),
            role: matched.role,
        });
        return Ok(next.run(request).await);
    }

    // Try session cookie
    if let Some(email) = oauth::get_session_email(&session).await {
        debug!(
            email,
            path = request.uri().path(),
            "authenticated via session"
        );
        request.extensions_mut().insert(Principal {
            label: email,
            role: Role::Reviewer,
        });
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// Route layer for reviewer-only endpoints. Must run inside [`require_auth`]
/// (or the anonymous `Principal` extension layer).
pub async fn require_reviewer(request: Request, next: Next) -> Result<Response, StatusCode> {
    match request.extensions().get::<Principal>() {
        Some(p) if p.role == Role::Reviewer => Ok(next.run(request).await),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Web auth middleware: session cookie only, redirects to login on failure.
pub async fn require_web_auth(session: Session, request: Request, next: Next) -> Response {
    if oauth::get_session_email(&session).await.is_some() {
        return next.run(request).await;
    }
    Redirect::temporary("/auth/login").into_response()
}

use std::env;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreUserInfoClaims};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, RedirectUrl, Scope, TokenResponse,
};
use serde::Deserialize;
use tower_sessions::Session;
use tracing::{error, info, warn};

use super::AppState;
use super::notifier::Notifier;
use crate::error::AppError;

/// Concrete client type returned by `from_provider_metadata` + `set_redirect_uri`.
type ProviderClient = CoreClient<
    EndpointSet,                   // HasAuthUrl
    openidconnect::EndpointNotSet, // HasDeviceAuthUrl
    openidconnect::EndpointNotSet, // HasIntrospectionUrl
    openidconnect::EndpointNotSet, // HasRevocationUrl
    EndpointMaybeSet,              // HasTokenUrl
    EndpointMaybeSet,              // HasUserInfoUrl
>;

/// An OIDC provider configuration.
pub struct OidcProvider {
    pub name: String,
    pub client: ProviderClient,
}

/// Basic auth credentials for development/testing.
pub struct BasicAuthCreds {
    pub user: String,
    pub password: String,
}

/// Manages all configured auth providers (OIDC + optional basic auth).
pub struct OAuthManager {
    pub providers: Vec<OidcProvider>,
    pub allowed_emails: Vec<EmailPattern>,
    pub basic_auth: Option<BasicAuthCreds>,
}

/// Email pattern for allowlist: exact match or domain wildcard (*@domain.com).
#[derive(Clone)]
pub enum EmailPattern {
    Exact(String),
    Domain(String),
}

impl EmailPattern {
    pub fn parse(s: &str) -> Self {
        if let Some(domain) = s.strip_prefix("*@") {
            EmailPattern::Domain(domain.to_lowercase())
        } else {
            EmailPattern::Exact(s.to_lowercase())
        }
    }

    pub fn matches(&self, email: &str) -> bool {
        let email = email.to_lowercase();
        match self {
            EmailPattern::Exact(e) => *e == email,
            EmailPattern::Domain(d) => email.ends_with(&format!("@{d}")),
        }
    }
}

impl OAuthManager {
    /// Build from environment variables. Returns None if no providers configured.
    pub async fn from_env(base_url: &str) -> Result<Option<Self>, AppError> {
        let mut providers = Vec::new();

        // Google OIDC
        if let (Ok(client_id), Ok(client_secret)) = (
            env::var("GOOGLE_CLIENT_ID"),
            env::var("GOOGLE_CLIENT_SECRET"),
        ) {
            let provider = build_provider(
                "google",
                "https://accounts.google.com",
                &client_id,
                &client_secret,
                &format!("{base_url}/auth/callback/google"),
            )
            .await?;
            providers.push(provider);
            info!("OAuth: Google OIDC configured");
        }

        // Custom OIDC
        if let (Ok(issuer_url), Ok(client_id), Ok(client_secret)) = (
            env::var("OIDC_ISSUER_URL"),
            env::var("OIDC_CLIENT_ID"),
            env::var("OIDC_CLIENT_SECRET"),
        ) {
            let provider = build_provider(
                "oidc",
                &issuer_url,
                &client_id,
                &client_secret,
                &format!("{base_url}/auth/callback/oidc"),
            )
            .await?;
            providers.push(provider);
            info!("OAuth: Custom OIDC configured");
        }

        // Basic auth (for development/testing only)
        let basic_auth = if let (Ok(user), Ok(password)) =
            (env::var("BASIC_AUTH_USER"), env::var("BASIC_AUTH_PASSWORD"))
        {
            warn!("Basic auth enabled — do NOT use in production");
            Some(BasicAuthCreds { user, password })
        } else {
            None
        };

        if providers.is_empty() && basic_auth.is_none() {
            return Ok(None);
        }

        let allowed_emails = env::var("OAUTH_ALLOWED_EMAILS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(EmailPattern::parse)
            .collect();

        Ok(Some(Self {
            providers,
            allowed_emails,
            basic_auth,
        }))
    }

    pub fn provider(&self, name: &str) -> Option<&OidcProvider> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn is_email_allowed(&self, email: &str) -> bool {
        if self.allowed_emails.is_empty() {
            return true;
        }
        self.allowed_emails.iter().any(|p| p.matches(email))
    }

    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name.as_str()).collect()
    }

    pub fn has_basic_auth(&self) -> bool {
        self.basic_auth.is_some()
    }

    pub fn check_basic_auth(&self, user: &str, password: &str) -> bool {
        self.basic_auth
            .as_ref()
            .is_some_and(|c| c.user == user && c.password == password)
    }
}

async fn build_provider(
    name: &str,
    issuer_url: &str,
    client_id: &str,
    client_secret: &str,
    redirect_url: &str,
) -> Result<OidcProvider, AppError> {
    let issuer = IssuerUrl::new(issuer_url.to_string())
        .map_err(|e| AppError::Config(format!("invalid issuer URL for {name}: {e}")))?;

    let http_client = reqwest::Client::new();
    let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
        .await
        .map_err(|e| AppError::Config(format!("OIDC discovery failed for {name}: {e}")))?;

    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(client_id.to_string()),
        Some(ClientSecret::new(client_secret.to_string())),
    )
    .set_redirect_uri(
        RedirectUrl::new(redirect_url.to_string())
            .map_err(|e| AppError::Config(format!("invalid redirect URL for {name}: {e}")))?,
    );

    Ok(OidcProvider {
        name: name.to_string(),
        client,
    })
}

// Session keys
const SESSION_EMAIL_KEY: &str = "user_email";
const SESSION_CSRF_KEY: &str = "oauth_csrf";
const SESSION_NONCE_KEY: &str = "oauth_nonce";

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

/// GET /auth/start/{provider} — initiate OAuth redirect.
pub async fn start_auth<N: Notifier>(
    State(state): State<AppState<N>>,
    session: Session,
    Path(provider_name): Path<String>,
) -> Response {
    let oauth = match state.oauth.as_ref() {
        Some(mgr) => mgr,
        None => return (StatusCode::NOT_FOUND, "OAuth not configured").into_response(),
    };

    let provider = match oauth.provider(&provider_name) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "Unknown provider").into_response(),
    };

    let (auth_url, csrf_token, nonce) = provider
        .client
        .authorize_url(
            AuthenticationFlow::<openidconnect::core::CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .url();

    if let Err(e) = session
        .insert(SESSION_CSRF_KEY, csrf_token.secret().to_string())
        .await
    {
        error!("failed to store CSRF in session: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }
    if let Err(e) = session
        .insert(SESSION_NONCE_KEY, nonce.secret().to_string())
        .await
    {
        error!("failed to store nonce in session: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }

    Redirect::temporary(auth_url.as_str()).into_response()
}

/// GET /auth/callback/{provider} — handle OAuth callback.
pub async fn callback<N: Notifier>(
    State(state): State<AppState<N>>,
    session: Session,
    Path(provider_name): Path<String>,
    Query(params): Query<CallbackParams>,
) -> Response {
    let oauth = match state.oauth.as_ref() {
        Some(mgr) => mgr,
        None => return (StatusCode::NOT_FOUND, "OAuth not configured").into_response(),
    };

    let provider = match oauth.provider(&provider_name) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "Unknown provider").into_response(),
    };

    // Verify CSRF
    let stored_csrf: Option<String> = session.get(SESSION_CSRF_KEY).await.unwrap_or(None);
    if stored_csrf.as_deref() != Some(&params.state) {
        warn!("OAuth CSRF mismatch");
        return (StatusCode::BAD_REQUEST, "CSRF validation failed").into_response();
    }

    let stored_nonce: Option<String> = session.get(SESSION_NONCE_KEY).await.unwrap_or(None);
    let nonce = match stored_nonce {
        Some(n) => Nonce::new(n),
        None => {
            warn!("Missing nonce in session");
            return (StatusCode::BAD_REQUEST, "Missing nonce").into_response();
        }
    };

    // Exchange code for tokens
    let http_client = reqwest::Client::new();
    let token_response = match provider
        .client
        .exchange_code(AuthorizationCode::new(params.code))
    {
        Ok(req) => match req.request_async(&http_client).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("Token exchange request failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Token exchange failed")
                    .into_response();
            }
        },
        Err(e) => {
            error!("Token exchange config error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Token exchange failed").into_response();
        }
    };

    // Extract and verify ID token
    let id_token = match token_response.id_token() {
        Some(t) => t,
        None => {
            error!("No ID token in response");
            return (StatusCode::INTERNAL_SERVER_ERROR, "No ID token").into_response();
        }
    };

    let verifier = provider.client.id_token_verifier();
    let claims = match id_token.claims(&verifier, &nonce) {
        Ok(c) => c,
        Err(e) => {
            error!("ID token verification failed: {e}");
            return (StatusCode::UNAUTHORIZED, "Token verification failed").into_response();
        }
    };

    // Try email from ID token first, fall back to userinfo endpoint
    let mut email = claims.email().map(|e| e.to_string()).unwrap_or_default();

    if email.is_empty() {
        let access_token = token_response.access_token().clone();
        let subject = Some(claims.subject().clone());
        match provider.client.user_info(access_token, subject) {
            Ok(req) => {
                let result: Result<CoreUserInfoClaims, _> = req.request_async(&http_client).await;
                match result {
                    Ok(userinfo) => {
                        email = userinfo.email().map(|e| e.to_string()).unwrap_or_default();
                    }
                    Err(e) => {
                        error!("Userinfo request failed: {e}");
                    }
                }
            }
            Err(e) => {
                error!("Userinfo endpoint not configured: {e}");
            }
        }
    }

    if email.is_empty() {
        return (StatusCode::UNAUTHORIZED, "No email in token or userinfo").into_response();
    }

    if !oauth.is_email_allowed(&email) {
        warn!(email, "OAuth login denied: email not in allowlist");
        return (StatusCode::FORBIDDEN, "Email not authorized").into_response();
    }

    if let Err(e) = session.insert(SESSION_EMAIL_KEY, &email).await {
        error!("Failed to store email in session: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }

    let _ = session.remove::<String>(SESSION_CSRF_KEY).await;
    let _ = session.remove::<String>(SESSION_NONCE_KEY).await;

    info!(email, "OAuth login successful");
    Redirect::to("/approvals").into_response()
}

/// POST /auth/logout
pub async fn logout(session: Session) -> Response {
    let _ = session.delete().await;
    Redirect::to("/auth/login").into_response()
}

/// Extract authenticated email from session. Returns None if not logged in.
pub async fn get_session_email(session: &Session) -> Option<String> {
    session
        .get::<String>(SESSION_EMAIL_KEY)
        .await
        .ok()
        .flatten()
}

/// Store authenticated email in session.
pub async fn set_session_email(
    session: &Session,
    email: &str,
) -> Result<(), tower_sessions::session::Error> {
    session.insert(SESSION_EMAIL_KEY, email).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_pattern_exact() {
        let p = EmailPattern::parse("nick@example.com");
        assert!(p.matches("nick@example.com"));
        assert!(p.matches("Nick@Example.com"));
        assert!(!p.matches("other@example.com"));
    }

    #[test]
    fn email_pattern_domain() {
        let p = EmailPattern::parse("*@company.com");
        assert!(p.matches("nick@company.com"));
        assert!(p.matches("anyone@Company.COM"));
        assert!(!p.matches("nick@other.com"));
    }
}

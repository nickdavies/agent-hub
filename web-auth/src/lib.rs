use std::env;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreUserInfoClaims};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, RedirectUrl, Scope, TokenResponse,
};
use protocol::Secret;
use serde::Deserialize;
use tower_sessions::Session;
use tracing::{error, info, warn};

type ProviderClient = CoreClient<
    EndpointSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    openidconnect::EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub struct OidcProvider {
    pub name: String,
    client: ProviderClient,
}

pub struct BasicAuthCreds {
    user: String,
    password: Secret,
}

pub struct OAuthManager {
    providers: Vec<OidcProvider>,
    allowed_emails: Vec<EmailPattern>,
    basic_auth: Option<BasicAuthCreds>,
}

#[derive(Clone)]
enum EmailPattern {
    Exact(String),
    Domain(String),
}

impl EmailPattern {
    fn parse(value: &str) -> Self {
        if let Some(domain) = value.strip_prefix("*@") {
            Self::Domain(domain.to_lowercase())
        } else {
            Self::Exact(value.to_lowercase())
        }
    }

    fn matches(&self, email: &str) -> bool {
        let email = email.to_lowercase();
        match self {
            Self::Exact(expected) => *expected == email,
            Self::Domain(domain) => email.ends_with(&format!("@{domain}")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

/// Authentication mode shared by the HTTP services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Token,
    None,
}

pub struct Token {
    pub label: String,
    pub secret: Secret,
}

pub fn parse_auth_config(
    auth_mode: Option<&str>,
    raw_tokens: Option<&str>,
) -> Result<(AuthMode, Vec<Token>), ConfigError> {
    let auth_mode = match auth_mode.unwrap_or("") {
        "" | "token" => AuthMode::Token,
        "none" => AuthMode::None,
        value => return Err(ConfigError(format!("unknown AUTH_MODE value '{value}'"))),
    };
    let tokens = if auth_mode == AuthMode::Token {
        let raw = raw_tokens.ok_or_else(|| ConfigError("AGENT_HUB_TOKENS not set".into()))?;
        let tokens = parse_tokens(raw)?;
        if tokens.is_empty() {
            return Err(ConfigError("AGENT_HUB_TOKENS is empty".into()));
        }
        tokens
    } else {
        Vec::new()
    };
    Ok((auth_mode, tokens))
}

fn parse_tokens(raw: &str) -> Result<Vec<Token>, ConfigError> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|entry| {
            let (label, secret) = entry.split_once(':').unwrap_or((entry, entry));
            if secret.is_empty() {
                return Err(ConfigError(format!(
                    "token with label '{label}' has empty secret"
                )));
            }
            Ok(Token {
                label: label.into(),
                secret: Secret::new(secret),
            })
        })
        .collect()
}

pub fn match_bearer_token<'a>(header: &str, tokens: &'a [Token]) -> Option<&'a Token> {
    let candidate = bearer_token(header)?;
    tokens
        .iter()
        .find(|token| constant_time_eq(token.secret.expose().as_bytes(), candidate.as_bytes()))
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

impl OAuthManager {
    pub async fn from_env(base_url: &str) -> Result<Option<Self>, ConfigError> {
        let mut providers = Vec::new();
        if let (Ok(client_id), Ok(client_secret)) = (
            env::var("GOOGLE_CLIENT_ID"),
            env::var("GOOGLE_CLIENT_SECRET"),
        ) {
            providers.push(
                build_provider(
                    "google",
                    "https://accounts.google.com",
                    &client_id,
                    &client_secret,
                    &format!("{base_url}/auth/callback/google"),
                )
                .await?,
            );
            info!("OAuth: Google OIDC configured");
        }
        if let (Ok(issuer_url), Ok(client_id), Ok(client_secret)) = (
            env::var("OIDC_ISSUER_URL"),
            env::var("OIDC_CLIENT_ID"),
            env::var("OIDC_CLIENT_SECRET"),
        ) {
            providers.push(
                build_provider(
                    "oidc",
                    &issuer_url,
                    &client_id,
                    &client_secret,
                    &format!("{base_url}/auth/callback/oidc"),
                )
                .await?,
            );
            info!("OAuth: Custom OIDC configured");
        }
        let basic_auth = if let (Ok(user), Ok(password)) =
            (env::var("BASIC_AUTH_USER"), env::var("BASIC_AUTH_PASSWORD"))
        {
            warn!("Basic auth enabled — do NOT use in production");
            Some(BasicAuthCreds {
                user,
                password: Secret::new(password),
            })
        } else {
            None
        };
        if providers.is_empty() && basic_auth.is_none() {
            return Ok(None);
        }
        let allowed_emails = env::var("OAUTH_ALLOWED_EMAILS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(EmailPattern::parse)
            .collect();
        Ok(Some(Self {
            providers,
            allowed_emails,
            basic_auth,
        }))
    }

    pub fn provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect()
    }

    pub fn has_basic_auth(&self) -> bool {
        self.basic_auth.is_some()
    }

    pub fn check_basic_auth(&self, user: &str, password: &str) -> bool {
        self.basic_auth.as_ref().is_some_and(|credentials| {
            constant_time_eq(credentials.user.as_bytes(), user.as_bytes())
                && constant_time_eq(
                    credentials.password.expose().as_bytes(),
                    password.as_bytes(),
                )
        })
    }

    fn provider(&self, name: &str) -> Option<&OidcProvider> {
        self.providers.iter().find(|provider| provider.name == name)
    }

    fn is_email_allowed(&self, email: &str) -> bool {
        self.allowed_emails.is_empty()
            || self
                .allowed_emails
                .iter()
                .any(|pattern| pattern.matches(email))
    }
}

async fn build_provider(
    name: &str,
    issuer_url: &str,
    client_id: &str,
    client_secret: &str,
    redirect_url: &str,
) -> Result<OidcProvider, ConfigError> {
    let issuer = IssuerUrl::new(issuer_url.to_string())
        .map_err(|error| ConfigError(format!("invalid issuer URL for {name}: {error}")))?;
    let http_client = reqwest::Client::new();
    let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
        .await
        .map_err(|error| ConfigError(format!("OIDC discovery failed for {name}: {error}")))?;
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(client_id.to_string()),
        Some(ClientSecret::new(client_secret.to_string())),
    )
    .set_redirect_uri(
        RedirectUrl::new(redirect_url.to_string())
            .map_err(|error| ConfigError(format!("invalid redirect URL for {name}: {error}")))?,
    );
    Ok(OidcProvider {
        name: name.to_string(),
        client,
    })
}

const SESSION_EMAIL_KEY: &str = "user_email";
const SESSION_CSRF_KEY: &str = "oauth_csrf";
const SESSION_NONCE_KEY: &str = "oauth_nonce";

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

pub async fn start_auth(
    oauth: Option<&OAuthManager>,
    session: Session,
    provider_name: &str,
) -> Response {
    let Some(oauth) = oauth else {
        return (StatusCode::NOT_FOUND, "OAuth not configured").into_response();
    };
    let Some(provider) = oauth.provider(provider_name) else {
        return (StatusCode::NOT_FOUND, "Unknown provider").into_response();
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
    if let Err(error) = session
        .insert(SESSION_CSRF_KEY, csrf_token.secret().to_string())
        .await
    {
        error!("failed to store CSRF in session: {error}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }
    if let Err(error) = session
        .insert(SESSION_NONCE_KEY, nonce.secret().to_string())
        .await
    {
        error!("failed to store nonce in session: {error}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }
    Redirect::temporary(auth_url.as_str()).into_response()
}

pub async fn callback(
    oauth: Option<&OAuthManager>,
    session: Session,
    provider_name: &str,
    params: CallbackParams,
    success_redirect: &'static str,
) -> Response {
    let Some(oauth) = oauth else {
        return (StatusCode::NOT_FOUND, "OAuth not configured").into_response();
    };
    let Some(provider) = oauth.provider(provider_name) else {
        return (StatusCode::NOT_FOUND, "Unknown provider").into_response();
    };
    let stored_csrf: Option<String> = session.get(SESSION_CSRF_KEY).await.unwrap_or(None);
    if stored_csrf.as_deref() != Some(&params.state) {
        warn!("OAuth CSRF mismatch");
        return (StatusCode::BAD_REQUEST, "CSRF validation failed").into_response();
    }
    let Some(stored_nonce) = session
        .get::<String>(SESSION_NONCE_KEY)
        .await
        .unwrap_or(None)
    else {
        warn!("Missing nonce in session");
        return (StatusCode::BAD_REQUEST, "Missing nonce").into_response();
    };
    let nonce = Nonce::new(stored_nonce);
    let http_client = reqwest::Client::new();
    let token_response = match provider
        .client
        .exchange_code(AuthorizationCode::new(params.code))
    {
        Ok(request) => match request.request_async(&http_client).await {
            Ok(response) => response,
            Err(error) => {
                error!("Token exchange request failed: {error}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Token exchange failed")
                    .into_response();
            }
        },
        Err(error) => {
            error!("Token exchange config error: {error}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Token exchange failed").into_response();
        }
    };
    let Some(id_token) = token_response.id_token() else {
        error!("No ID token in response");
        return (StatusCode::INTERNAL_SERVER_ERROR, "No ID token").into_response();
    };
    let claims = match id_token.claims(&provider.client.id_token_verifier(), &nonce) {
        Ok(claims) => claims,
        Err(error) => {
            error!("ID token verification failed: {error}");
            return (StatusCode::UNAUTHORIZED, "Token verification failed").into_response();
        }
    };
    let mut email = claims
        .email()
        .map(|email| email.to_string())
        .unwrap_or_default();
    if email.is_empty() {
        match provider.client.user_info(
            token_response.access_token().clone(),
            Some(claims.subject().clone()),
        ) {
            Ok(request) => {
                let result: Result<CoreUserInfoClaims, _> =
                    request.request_async(&http_client).await;
                match result {
                    Ok(userinfo) => {
                        email = userinfo
                            .email()
                            .map(|email| email.to_string())
                            .unwrap_or_default();
                    }
                    Err(error) => error!("Userinfo request failed: {error}"),
                }
            }
            Err(error) => error!("Userinfo endpoint not configured: {error}"),
        }
    }
    if email.is_empty() {
        return (StatusCode::UNAUTHORIZED, "No email in token or userinfo").into_response();
    }
    if !oauth.is_email_allowed(&email) {
        warn!(email, "OAuth login denied: email not in allowlist");
        return (StatusCode::FORBIDDEN, "Email not authorized").into_response();
    }
    if let Err(error) = set_session_email(&session, &email).await {
        error!("Failed to store email in session: {error}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }
    let _ = session.remove::<String>(SESSION_CSRF_KEY).await;
    let _ = session.remove::<String>(SESSION_NONCE_KEY).await;
    info!(email, "OAuth login successful");
    Redirect::to(success_redirect).into_response()
}

pub async fn logout(session: Session) -> Response {
    let _ = session.delete().await;
    Redirect::to("/auth/login").into_response()
}

pub async fn get_session_email(session: &Session) -> Option<String> {
    session
        .get::<String>(SESSION_EMAIL_KEY)
        .await
        .ok()
        .flatten()
}

pub async fn set_session_email(
    session: &Session,
    email: &str,
) -> Result<(), tower_sessions::session::Error> {
    session.insert(SESSION_EMAIL_KEY, email).await
}

pub fn bearer_token(header: &str) -> Option<&str> {
    header.strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_allowlist_matches_exact_and_domain_case_insensitively() {
        let exact = EmailPattern::parse("nick@example.com");
        assert!(exact.matches("Nick@Example.com"));
        assert!(!exact.matches("other@example.com"));
        let domain = EmailPattern::parse("*@company.com");
        assert!(domain.matches("anyone@Company.COM"));
        assert!(!domain.matches("nick@other.com"));
    }

    #[test]
    fn bearer_token_requires_canonical_scheme() {
        assert_eq!(bearer_token("Bearer secret"), Some("secret"));
        assert_eq!(bearer_token("bearer secret"), None);
        assert_eq!(bearer_token("secret"), None);
    }

    #[test]
    fn auth_config_rejects_unknown_mode_and_parses_labeled_tokens() {
        assert!(parse_auth_config(Some("typo"), Some("token")).is_err());
        let (mode, tokens) =
            parse_auth_config(Some("token"), Some("desktop:secret,plain")).unwrap();
        assert_eq!(mode, AuthMode::Token);
        assert_eq!(tokens[0].label, "desktop");
        assert_eq!(tokens[0].secret.expose(), "secret");
        assert_eq!(tokens[1].label, "plain");
    }

    #[test]
    fn bearer_matching_uses_exact_secret_bytes() {
        let (_, tokens) = parse_auth_config(Some("token"), Some("desktop:secret")).unwrap();
        assert_eq!(
            match_bearer_token("Bearer secret", &tokens).unwrap().label,
            "desktop"
        );
        assert!(match_bearer_token("Bearer secre", &tokens).is_none());
        assert!(match_bearer_token("Bearer secret-extra", &tokens).is_none());
    }
}

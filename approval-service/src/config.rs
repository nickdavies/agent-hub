use std::env;
use std::sync::Arc;

use tokio::sync::RwLock;

// Re-export protocol types so existing `use super::config::X` imports work.
pub use protocol::{NotifyConfig, NotifyConfigUpdate, Secret};

use super::sessions::SessionApprovalMode;
use crate::error::AppError;
pub use agent_hub_web_auth::{AuthMode, Token};

/// Feature mode for the approval system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalFeatureMode {
    Disabled,
    Readonly,
    Readwrite,
}

/// Immutable server configuration loaded from environment at startup.
pub struct ServerConfig {
    pub auth_mode: AuthMode,
    pub tokens: Vec<Token>,
    pub listen_addr: String,
    pub presence_ttl_secs: u64,
    pub session_ttl_secs: u64,
    pub notification_delay_secs: u64,
    pub approval_mode: ApprovalFeatureMode,
    pub base_url: Option<String>,
    pub default_approval_mode: SessionApprovalMode,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, AppError> {
        let auth_mode_value = env::var("AUTH_MODE").ok();
        let tokens_value = env::var("AGENT_HUB_TOKENS").ok();
        let (auth_mode, tokens) = agent_hub_web_auth::parse_auth_config(
            auth_mode_value.as_deref(),
            tokens_value.as_deref(),
        )
        .map_err(|error| AppError::Config(error.to_string()))?;

        let default_addr = match auth_mode {
            AuthMode::None => "127.0.0.1:8080",
            AuthMode::Token => "0.0.0.0:8080",
        };
        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| default_addr.into());
        let presence_ttl_secs = parse_optional_u64("PRESENCE_TTL", 120)?;
        let session_ttl_secs = parse_optional_u64("SESSION_TTL", 7200)?;
        let notification_delay_secs = parse_optional_u64("NOTIFICATION_DELAY", 0)?;

        let approval_mode = parse_approval_mode(&env::var("APPROVAL_MODE").unwrap_or_default())?;

        let base_url = env::var("BASE_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string());

        if approval_mode != ApprovalFeatureMode::Disabled && base_url.is_none() {
            return Err(AppError::Config(
                "BASE_URL is required when APPROVAL_MODE is not disabled".into(),
            ));
        }

        let default_approval_mode =
            parse_default_approval_mode(optional_env("DEFAULT_APPROVAL_MODE")?.as_deref())?;

        Ok(Self {
            auth_mode,
            tokens,
            listen_addr,
            presence_ttl_secs,
            session_ttl_secs,
            notification_delay_secs,
            approval_mode,
            base_url,
            default_approval_mode,
        })
    }
}

fn optional_env(name: &str) -> Result<Option<String>, AppError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(AppError::Config(format!("invalid {name}: {error}"))),
    }
}

fn parse_optional_u64(name: &str, default: u64) -> Result<u64, AppError> {
    match optional_env(name)? {
        None => Ok(default),
        Some(value) => value
            .parse()
            .map_err(|_| AppError::Config(format!("invalid {name} value '{value}'"))),
    }
}

fn parse_default_approval_mode(value: Option<&str>) -> Result<SessionApprovalMode, AppError> {
    match value {
        None | Some("remote") => Ok(SessionApprovalMode::Remote),
        Some("terminal") => Ok(SessionApprovalMode::Terminal),
        Some(value) => Err(AppError::Config(format!(
            "unknown DEFAULT_APPROVAL_MODE value '{value}'"
        ))),
    }
}

fn parse_approval_mode(value: &str) -> Result<ApprovalFeatureMode, AppError> {
    Ok(match value {
        "" | "disabled" => ApprovalFeatureMode::Disabled,
        "readonly" => ApprovalFeatureMode::Readonly,
        "readwrite" => ApprovalFeatureMode::Readwrite,
        value => {
            return Err(AppError::Config(format!(
                "unknown APPROVAL_MODE value '{value}'"
            )));
        }
    })
}

/// Shared mutable notify config.
pub type SharedNotifyConfig = Arc<RwLock<NotifyConfig>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_labeled_tokens() {
        let (_, tokens) = agent_hub_web_auth::parse_auth_config(
            Some("token"),
            Some("desktop:abc123,mobile:def456"),
        )
        .unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "desktop");
        assert_eq!(tokens[0].secret.expose(), "abc123");
        assert_eq!(tokens[1].label, "mobile");
        assert_eq!(tokens[1].secret.expose(), "def456");
    }

    #[test]
    fn parse_plain_tokens() {
        let (_, tokens) =
            agent_hub_web_auth::parse_auth_config(Some("token"), Some("abc123,def456")).unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "abc123");
        assert_eq!(tokens[0].secret.expose(), "abc123");
    }

    #[test]
    fn parse_empty_secret_fails() {
        assert!(agent_hub_web_auth::parse_auth_config(Some("token"), Some("desktop:")).is_err());
    }

    #[test]
    fn unknown_approval_mode_fails() {
        assert!(parse_approval_mode("read-write").is_err());
    }

    #[test]
    fn default_approval_mode_is_strict_and_missing_uses_remote() {
        assert_eq!(
            parse_default_approval_mode(None).unwrap(),
            SessionApprovalMode::Remote
        );
        assert_eq!(
            parse_default_approval_mode(Some("terminal")).unwrap(),
            SessionApprovalMode::Terminal
        );
        assert!(parse_default_approval_mode(Some("automatic")).is_err());
        assert!(parse_default_approval_mode(Some("")).is_err());
    }

    #[test]
    fn notify_config_partial_update() {
        let mut cfg = NotifyConfig::with_delay(0);
        cfg.apply(NotifyConfigUpdate {
            stop_enabled: Some(false),
            permission_enabled: None,
            notification_delay_secs: Some(30),
        });
        assert!(!cfg.stop_enabled);
        assert!(cfg.permission_enabled);
        assert_eq!(cfg.notification_delay_secs, 30);
    }
}

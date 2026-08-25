use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use protocol::{EnvironmentDefinition, EnvironmentOverlay};
use tokio::sync::RwLock;

// Re-export protocol types so existing `use super::config::X` imports work.
pub use protocol::{NotifyConfig, NotifyConfigUpdate, Secret};

use super::sessions::SessionApprovalMode;
use crate::error::AppError;

pub const GITHUB_WEBHOOK_SECRET_ENV: &str = "AGENT_HUB_GITHUB_WEBHOOK_SECRET";

/// Feature mode for the approval system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalFeatureMode {
    Disabled,
    Readonly,
    Readwrite,
}

/// Authentication mode for the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// Bearer token authentication required.
    Token,
    /// No authentication. Intended for localhost / Docker use only.
    None,
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
    pub workflows_dir: String,
    pub config_root: String,
    pub task_dir: String,
    pub task_state_dir: String,
    pub task_adapter: String,
    #[allow(dead_code)]
    pub vikunja_base_url: Option<String>,
    #[allow(dead_code)]
    pub vikunja_token: Option<String>,
    #[allow(dead_code)]
    pub vikunja_bot_username: Option<String>,
    #[allow(dead_code)]
    pub vikunja_bot_password: Option<String>,
    #[allow(dead_code)]
    pub vikunja_project_id: Option<i64>,
    #[allow(dead_code)]
    pub vikunja_label_prefix: Option<String>,
    pub orchestration_db_path: String,
    pub artifact_store_root: String,
    pub grpc_listen_addr: Option<String>,
    pub github_webhook_secret: Option<String>,
    pub github_polling_enabled: bool,
    pub github_poll_interval_secs: u64,
}

pub struct Token {
    pub label: String,
    pub secret: Secret,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, AppError> {
        let task_adapter = env::var("TASK_ADAPTER").unwrap_or_else(|_| "file".into());
        validate_task_adapter(&task_adapter)?;
        let task_state_dir = env::var("TASK_STATE_DIR")
            .map_err(|_| AppError::Config("TASK_STATE_DIR is required for file mode".into()))?;
        let auth_mode = match env::var("AUTH_MODE").unwrap_or_default().as_str() {
            "none" => AuthMode::None,
            _ => AuthMode::Token,
        };

        let tokens = if auth_mode == AuthMode::Token {
            let raw_tokens = env::var("AGENT_HUB_TOKENS")
                .map_err(|_| AppError::Config("AGENT_HUB_TOKENS not set".into()))?;
            let tokens = parse_tokens(&raw_tokens)?;
            if tokens.is_empty() {
                return Err(AppError::Config("AGENT_HUB_TOKENS is empty".into()));
            }
            tokens
        } else {
            Vec::new()
        };

        let default_addr = match auth_mode {
            AuthMode::None => "127.0.0.1:8080",
            AuthMode::Token => "0.0.0.0:8080",
        };
        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| default_addr.into());
        let presence_ttl_secs = env::var("PRESENCE_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let session_ttl_secs = env::var("SESSION_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7200);
        let notification_delay_secs = env::var("NOTIFICATION_DELAY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let approval_mode = match env::var("APPROVAL_MODE").unwrap_or_default().as_str() {
            "readonly" => ApprovalFeatureMode::Readonly,
            "readwrite" => ApprovalFeatureMode::Readwrite,
            _ => ApprovalFeatureMode::Disabled,
        };

        let base_url = env::var("BASE_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string());

        if approval_mode != ApprovalFeatureMode::Disabled && base_url.is_none() {
            return Err(AppError::Config(
                "BASE_URL is required when APPROVAL_MODE is not disabled".into(),
            ));
        }

        let default_approval_mode = match env::var("DEFAULT_APPROVAL_MODE")
            .unwrap_or_default()
            .as_str()
        {
            "terminal" => SessionApprovalMode::Terminal,
            _ => SessionApprovalMode::Remote,
        };

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
            workflows_dir: env::var("WORKFLOWS_DIR").unwrap_or_else(|_| "workflows".into()),
            config_root: env::var("CONFIG_ROOT")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "./config".to_string()),
            task_dir: env::var("TASK_DIR").unwrap_or_else(|_| "tasks".into()),
            task_state_dir,
            task_adapter,
            vikunja_base_url: env::var("VIKUNJA_BASE_URL").ok(),
            vikunja_token: env::var("VIKUNJA_TOKEN").ok(),
            vikunja_bot_username: env::var("VIKUNJA_BOT_USERNAME").ok(),
            vikunja_bot_password: env::var("VIKUNJA_BOT_PASSWORD").ok(),
            vikunja_project_id: env::var("VIKUNJA_PROJECT_ID")
                .ok()
                .and_then(|v| v.parse().ok()),
            vikunja_label_prefix: env::var("VIKUNJA_LABEL_PREFIX").ok(),
            orchestration_db_path: env::var("ORCHESTRATION_DB")
                .unwrap_or_else(|_| "orchestration.sqlite3".into()),
            artifact_store_root: env::var("ARTIFACT_STORE_ROOT")
                .unwrap_or_else(|_| "local-agent-data".into()),
            grpc_listen_addr: env::var("GRPC_LISTEN_ADDR").ok().and_then(|v| {
                let trimmed = v.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }),
            github_webhook_secret: env::var(GITHUB_WEBHOOK_SECRET_ENV).ok().and_then(|v| {
                let trimmed = v.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }),
            github_polling_enabled: env::var("GITHUB_POLLING_ENABLED")
                .ok()
                .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false),
            github_poll_interval_secs: env::var("GITHUB_POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        })
    }
}

fn validate_task_adapter(value: &str) -> Result<(), AppError> {
    match value {
        "file" => Ok(()),
        "vikunja" => Err(AppError::Config(
            "TASK_ADAPTER=vikunja is temporarily unsupported; this milestone is file-only".into(),
        )),
        _ => Err(AppError::Config(
            "TASK_ADAPTER must be exactly 'file' for this milestone".into(),
        )),
    }
}

#[derive(serde::Deserialize)]
struct SystemManifestFile {
    system: SystemManifest,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SystemManifest {
    pub operator: Option<String>,
    pub github: Option<SystemGithubConfig>,
    /// SCM hosts and their authoritative read-only observation credentials.
    #[serde(default)]
    pub scm_hosts: Vec<ScmHostConfig>,
    pub network_policies: NetworkPoliciesConfig,
    pub environments: EnvironmentsConfig,
    pub workflows: Option<WorkflowsConfig>,
    pub repo_discovery: Option<RepoDiscoveryConfig>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemGithubConfig {
    pub webhook_secret_env: String,
    #[serde(rename = "poll_interval_secs")]
    pub _poll_interval_secs: u64,
    #[serde(rename = "owner_login")]
    pub _owner_login: String,
}

/// Source-control-management host registered with the orchestrator.
///
/// Each `[[system.scm_hosts]]` row describes one upstream SCM endpoint —
/// one GitHub instance, one GitLab instance, etc. The `kind` discriminator
/// picks which polling/integration backend handles the host. For now only
/// `kind = "github"` is implemented; new kinds plug in by adding another
/// `Poller` impl + a `kind` arm.
///
/// Tokens are deliberately *not* embedded in the TOML. We point at an
/// environment variable name (`read_token_env`) so secrets stay out of repo and
/// rotate independently of config.
///
/// Webhook authentication uses the canonical
/// `AGENT_HUB_GITHUB_WEBHOOK_SECRET` process environment variable.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScmHostConfig {
    pub kind: String,
    pub host: String,
    pub api_base_url: String,
    pub read_token_env: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NetworkPoliciesConfig {
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EnvironmentsConfig {
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowsConfig {
    pub installed: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedTriagePrompt {
    pub system: Option<String>,
    pub user: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoDiscoveryConfig {
    pub enabled: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    pub strategies: Vec<RepoDiscoveryStrategy>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoDiscoveryStrategy {
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub strategy_type: String,
    #[allow(dead_code)]
    pub path: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub include: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub exclude: Vec<String>,
}

#[derive(serde::Deserialize)]
struct EnvironmentFile {
    environment: EnvironmentToml,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EnvironmentToml {
    pub name: String,
    pub description: Option<String>,
    pub image: String,
    pub network_policy: String,
    pub approval_mode: String,
    #[serde(default)]
    pub allowed_overlays: Vec<String>,
    #[serde(default)]
    pub include_skills: Vec<String>,
    #[serde(default)]
    pub include_agents: Vec<String>,
    #[serde(default)]
    pub include_prompts: Vec<String>,
    pub max_timeout_secs: Option<u64>,
    /// Optional runtime preference (e.g. `opencode-default`,
    /// or `auto`). The local-agent honors this when dispatching agent jobs;
    /// the server simply projects it into job context.
    #[serde(default)]
    pub runtime: Option<String>,
}

pub type NetworkPolicyToml = agent_hub_config::NetworkPolicyConfig;

pub struct LoadedConfigTree {
    pub system: SystemManifest,
    pub environments: Vec<LoadedEnvironment>,
    pub network_policies: Vec<NetworkPolicyToml>,
    pub network_policy_file_digests:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    pub overlays: Vec<EnvironmentOverlay>,
    pub triage_prompt: Option<LoadedTriagePrompt>,
    pub runner_profiles: agent_hub_config::RunnerProfilesConfig,
    pub triage_mounts: Vec<agent_hub_config::TriageRepositoryMount>,
    pub environment_bundles:
        std::collections::BTreeMap<String, agent_hub_config::EnvironmentBundle>,
}

pub struct LoadedEnvironment {
    pub toml: EnvironmentToml,
}

impl LoadedEnvironment {
    pub fn to_definition(&self) -> EnvironmentDefinition {
        EnvironmentDefinition {
            name: self.toml.name.clone(),
            image: self.toml.image.clone(),
            network_class: self.toml.network_policy.clone(),
            approval_mode: self.toml.approval_mode.clone(),
            allowed_overlays: self.toml.allowed_overlays.clone(),
            description: self.toml.description.clone(),
            runtime: self
                .toml
                .runtime
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        }
    }
}

pub fn load_config_tree(root: &Path) -> anyhow::Result<LoadedConfigTree> {
    let deployment =
        agent_hub_config::validate_authoritative_config_root(root).map_err(anyhow::Error::msg)?;
    let system_file: SystemManifestFile = toml::from_str(&deployment.system_toml)
        .context("parse captured validated system manifest")?;
    let system = system_file.system;
    if let Some(github) = system.github.as_ref()
        && github.webhook_secret_env != GITHUB_WEBHOOK_SECRET_ENV
    {
        anyhow::bail!("system.github.webhook_secret_env must be {GITHUB_WEBHOOK_SECRET_ENV}");
    }
    let runner_profiles = deployment.runner_profiles.clone();

    let mut environments = Vec::new();
    for environment_name in &system.environments.installed {
        let bundle = deployment
            .environment_bundles
            .get(environment_name)
            .with_context(|| format!("validated bundle '{environment_name}' is unavailable"))?;
        let environment_bytes = bundle
            .files
            .iter()
            .find(|file| file.path == "environment.toml")
            .with_context(|| format!("bundle '{environment_name}' has no environment.toml"))?;
        let environment_file: EnvironmentFile = toml::from_str(
            std::str::from_utf8(&environment_bytes.bytes)
                .context("captured environment manifest is not UTF-8")?,
        )
        .context("parse captured validated environment manifest")?;

        environments.push(LoadedEnvironment {
            toml: environment_file.environment,
        });
    }

    let network_policies = system
        .network_policies
        .installed
        .iter()
        .map(|name| {
            deployment
                .network_policies
                .get(name)
                .map(|snapshot| snapshot.policy.clone())
                .with_context(|| format!("validated network policy '{name}' is unavailable"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let network_policy_file_digests = deployment
        .network_policies
        .iter()
        .map(|(name, snapshot)| (name.clone(), snapshot.file_digests.clone()))
        .collect();

    let overlays = deployment
        .overlay_registry
        .as_ref()
        .map(|registry| {
            registry
                .overlays
                .iter()
                .map(|entry| EnvironmentOverlay {
                    name: entry.name.clone(),
                    provider: entry.provider.clone(),
                    config: entry.config.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let triage_prompt = deployment
        .triage_prompt
        .as_ref()
        .map(|triage| LoadedTriagePrompt {
            system: Some(triage.prompt.system.clone()),
            user: triage.prompt.user.clone(),
        });

    Ok(LoadedConfigTree {
        system,
        environments,
        network_policies,
        overlays,
        triage_prompt,
        runner_profiles,
        triage_mounts: deployment.triage_mounts,
        environment_bundles: deployment.environment_bundles,
        network_policy_file_digests,
    })
}

fn parse_tokens(raw: &str) -> Result<Vec<Token>, AppError> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|entry| {
            if let Some((label, secret)) = entry.split_once(':') {
                if secret.is_empty() {
                    return Err(AppError::Config(format!(
                        "token with label '{label}' has empty secret"
                    )));
                }
                Ok(Token {
                    label: label.into(),
                    secret: Secret::new(secret),
                })
            } else {
                Ok(Token {
                    label: entry.into(),
                    secret: Secret::new(entry),
                })
            }
        })
        .collect()
}

/// Shared mutable notify config.
pub type SharedNotifyConfig = Arc<RwLock<NotifyConfig>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_only_adapter_boundary_rejects_vikunja_and_unknown_values() {
        assert!(validate_task_adapter("file").is_ok());
        assert!(
            validate_task_adapter("vikunja")
                .unwrap_err()
                .to_string()
                .contains("temporarily unsupported")
        );
        for value in ["", "FILE", "unknown"] {
            assert!(validate_task_adapter(value).is_err());
        }
    }

    #[test]
    fn parse_labeled_tokens() {
        let tokens = parse_tokens("desktop:abc123,mobile:def456").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "desktop");
        assert_eq!(tokens[0].secret.expose(), "abc123");
        assert_eq!(tokens[1].label, "mobile");
        assert_eq!(tokens[1].secret.expose(), "def456");
    }

    #[test]
    fn parse_plain_tokens() {
        let tokens = parse_tokens("abc123,def456").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "abc123");
        assert_eq!(tokens[0].secret.expose(), "abc123");
    }

    #[test]
    fn parse_empty_secret_fails() {
        assert!(parse_tokens("desktop:").is_err());
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

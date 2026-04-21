use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use protocol::{EnvironmentDefinition, EnvironmentOverlay};
use tokio::sync::RwLock;

// Re-export protocol types so existing `use super::config::X` imports work.
pub use protocol::{NotifyConfig, NotifyConfigUpdate, Secret};

use super::sessions::SessionApprovalMode;
use crate::error::AppError;

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
    pub task_adapter: String,
    pub vikunja_base_url: Option<String>,
    pub vikunja_token: Option<String>,
    pub vikunja_bot_username: Option<String>,
    pub vikunja_bot_password: Option<String>,
    pub vikunja_project_id: Option<i64>,
    pub vikunja_label_prefix: Option<String>,
    pub orchestration_db_path: String,
    pub artifact_store_root: String,
    pub grpc_listen_addr: Option<String>,
    pub github_webhook_secret: Option<String>,
    pub github_polling_enabled: bool,
    pub github_poll_interval_secs: u64,
    pub github_token: Option<String>,
    pub github_api_base_url: String,
}

pub struct Token {
    pub label: String,
    pub secret: Secret,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, AppError> {
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
            task_adapter: env::var("TASK_ADAPTER").unwrap_or_else(|_| "file".into()),
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
            github_webhook_secret: env::var("GITHUB_WEBHOOK_SECRET").ok().and_then(|v| {
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
            github_token: env::var("GITHUB_TOKEN").ok().and_then(|v| {
                let trimmed = v.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }),
            github_api_base_url: env::var("GITHUB_API_BASE_URL")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "https://api.github.com".to_string()),
        })
    }
}

#[derive(serde::Deserialize)]
struct SystemManifestFile {
    system: SystemManifest,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SystemManifest {
    pub operator: Option<String>,
    pub bot_identity: Option<String>,
    pub task_provider: Option<TaskProviderConfig>,
    pub github: Option<GithubConfig>,
    /// Optional SCM host registry. When empty, the legacy
    /// `GITHUB_TOKEN`/`GITHUB_API_BASE_URL` env vars synthesize a single
    /// implicit `kind = "github", host = "github.com"` row at startup so
    /// existing deployments keep working.
    #[serde(default)]
    pub scm_hosts: Vec<ScmHostConfig>,
    #[serde(default)]
    pub llm_providers: Vec<LlmProviderConfig>,
    pub images: ImagesConfig,
    pub network_policies: NetworkPoliciesConfig,
    pub environments: EnvironmentsConfig,
    pub workflows: Option<WorkflowsConfig>,
    pub overlays: Option<OverlaysConfig>,
    pub triage: Option<TriageConfig>,
    pub repo_discovery: Option<RepoDiscoveryConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    pub local_agent_defaults: Option<LocalAgentDefaultsConfig>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TaskProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubConfig {
    pub webhook_secret_env: Option<String>,
    pub poll_interval_secs: Option<u64>,
    pub owner_login: Option<String>,
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
/// environment variable name (`token_env`) so secrets stay out of repo and
/// rotate independently of config.
///
/// `webhook_secret_env` is intentionally absent today: the webhook handler
/// (`orchestration/webhook.rs`) is still single-host and uses the legacy
/// `GITHUB_WEBHOOK_SECRET` env var directly. When that handler grows
/// multi-host routing, add `webhook_secret_env` back here so its
/// presence/absence is validated at startup.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScmHostConfig {
    pub kind: String,
    pub host: String,
    pub api_base_url: String,
    pub token_env: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LlmProviderConfig {
    pub name: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ImagesConfig {
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NetworkPoliciesConfig {
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EnvironmentsConfig {
    pub installed: Vec<String>,
    pub triage_environment: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowsConfig {
    pub installed: Vec<String>,
    pub triage_rules: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OverlaysConfig {
    pub registry: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TriageConfig {
    /// Path (relative to the config root) of the triage prompt manifest.
    /// Used by the `__triage__` pseudo-workflow as the universal ingress
    /// route into any selectable workflow.
    pub config: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedTriagePrompt {
    pub system: Option<String>,
    pub user: String,
}

#[derive(serde::Deserialize)]
struct TriageManifestFile {
    prompt: TriagePromptToml,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TriagePromptToml {
    #[serde(default)]
    pub system: Option<String>,
    pub user: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoDiscoveryConfig {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub strategies: Vec<RepoDiscoveryStrategy>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepoDiscoveryStrategy {
    #[serde(rename = "type")]
    pub strategy_type: String,
    pub path: Option<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub supported_modes: Vec<String>,
    #[serde(default)]
    pub environments: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LocalAgentDefaultsConfig {
    pub mirror_dir: Option<String>,
    pub worktree_dir: Option<String>,
    pub transcript_dir: Option<String>,
    pub transcript_retention_days: Option<u64>,
    pub fetch_interval_secs: Option<u64>,
}

#[derive(serde::Deserialize)]
struct EnvironmentFile {
    environment: EnvironmentToml,
}

#[derive(Debug, Clone, serde::Deserialize)]
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
    /// Optional runtime preference (e.g. `cursor-default`, `opencode-default`,
    /// or `auto`). The local-agent honors this when dispatching agent jobs;
    /// the server simply projects it into job context.
    #[serde(default)]
    pub runtime: Option<String>,
}

#[derive(serde::Deserialize)]
struct NetworkPolicyFile {
    policy: NetworkPolicyToml,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NetworkPolicyToml {
    pub name: String,
    pub description: Option<String>,
    pub extends: Option<String>,
    pub docker_network: Option<String>,
    pub unrestricted: Option<bool>,
    #[serde(default)]
    pub allow: Vec<PolicyAllowEntry>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub hosts_from: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolicyAllowEntry {
    pub description: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    pub hosts_from: Option<String>,
}

#[derive(serde::Deserialize)]
struct ImageFile {
    image: ImageToml,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ImageToml {
    pub name: String,
    pub description: Option<String>,
    pub extends: Option<String>,
}

pub struct LoadedConfigTree {
    pub system: SystemManifest,
    pub environments: Vec<LoadedEnvironment>,
    pub network_policies: Vec<NetworkPolicyToml>,
    pub images: Vec<ImageToml>,
    pub overlays: Vec<EnvironmentOverlay>,
    pub triage_prompt: Option<LoadedTriagePrompt>,
}

pub struct LoadedEnvironment {
    pub toml: EnvironmentToml,
    pub dir: PathBuf,
    pub permissions_path: PathBuf,
    pub dippy_path: PathBuf,
    pub opencode_path: PathBuf,
}

impl LoadedEnvironment {
    pub fn to_definition(&self) -> EnvironmentDefinition {
        EnvironmentDefinition {
            name: self.toml.name.clone(),
            image: self.toml.image.clone(),
            network_class: map_policy_to_network_class(&self.toml.network_policy),
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

fn map_policy_to_network_class(policy: &str) -> String {
    match policy {
        "none" => "none".to_string(),
        "full" => "full".to_string(),
        _ => "restricted".to_string(),
    }
}

#[derive(serde::Deserialize)]
struct OverlaysFile {
    overlays: Vec<OverlayConfigEntry>,
}

#[derive(serde::Deserialize)]
struct OverlayConfigEntry {
    name: String,
    provider: String,
    #[serde(default)]
    config: serde_json::Value,
    #[allow(dead_code)]
    description: Option<String>,
}

pub fn load_config_tree(root: &Path) -> anyhow::Result<LoadedConfigTree> {
    let system_path = root.join("system.toml");
    let system_file: SystemManifestFile = read_toml_file(&system_path, "system manifest")?;
    let system = system_file.system;

    let mut environments = Vec::new();
    for environment_name in &system.environments.installed {
        let dir = root.join("environments").join(environment_name);
        ensure_exists(&dir, "environment directory")?;

        let environment_path = dir.join("environment.toml");
        let environment_file: EnvironmentFile =
            read_toml_file(&environment_path, "environment manifest")?;

        let permissions_path = dir.join("agent-hub-permissions.json");
        let dippy_path = dir.join("dippy.conf");
        let opencode_path = dir.join("opencode.json");
        ensure_exists(&permissions_path, "environment permissions")?;
        ensure_exists(&dippy_path, "environment dippy config")?;
        ensure_exists(&opencode_path, "environment opencode config")?;

        environments.push(LoadedEnvironment {
            toml: environment_file.environment,
            dir,
            permissions_path,
            dippy_path,
            opencode_path,
        });
    }

    let mut network_policies = Vec::new();
    for policy_name in &system.network_policies.installed {
        let policy_path = root
            .join("network_policies")
            .join(policy_name)
            .join("policy.toml");
        let policy_file: NetworkPolicyFile =
            read_toml_file(&policy_path, "network policy manifest")?;
        network_policies.push(policy_file.policy);
    }

    let mut images = Vec::new();
    for image_name in &system.images.installed {
        let image_path = root.join("images").join(image_name).join("image.toml");
        let image_file: ImageFile = read_toml_file(&image_path, "image manifest")?;
        images.push(image_file.image);
    }

    let overlays = if let Some(overlays) = &system.overlays {
        if let Some(registry_path) = overlays.registry.as_deref() {
            let absolute_registry_path = root.join(registry_path);
            let overlays_file: OverlaysFile =
                read_toml_file(&absolute_registry_path, "overlay registry")?;
            overlays_file
                .overlays
                .into_iter()
                .map(|entry| EnvironmentOverlay {
                    name: entry.name,
                    provider: entry.provider,
                    config: entry.config,
                })
                .collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let triage_prompt = if let Some(triage) = system.triage.as_ref()
        && let Some(config_path) = triage.config.as_deref()
    {
        let absolute_path = root.join(config_path);
        let manifest: TriageManifestFile = read_toml_file(&absolute_path, "triage prompt manifest")?;
        Some(LoadedTriagePrompt {
            system: manifest.prompt.system,
            user: manifest.prompt.user,
        })
    } else {
        None
    };

    Ok(LoadedConfigTree {
        system,
        environments,
        network_policies,
        images,
        overlays,
        triage_prompt,
    })
}

fn read_toml_file<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> anyhow::Result<T> {
    ensure_exists(path, label)?;
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {label} at {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse {label} at {}", path.display()))
}

fn ensure_exists(path: &Path, label: &str) -> anyhow::Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("missing {label} at {}", path.display()))
    }
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

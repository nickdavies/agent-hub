use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct SystemTomlFile {
    system: SystemConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemConfig {
    pub operator: String,
    pub bot_identity: String,
    #[serde(default)]
    pub task_provider: Option<SystemTaskProviderConfig>,
    #[serde(default)]
    pub github: Option<SystemGithubConfig>,
    #[serde(default)]
    pub llm_providers: Vec<SystemLlmProviderConfig>,
    #[serde(default)]
    pub images: Option<SystemImagesConfig>,
    #[serde(default)]
    pub network_policies: Option<SystemNetworkPoliciesConfig>,
    #[serde(default)]
    pub environments: Option<SystemEnvironmentsConfig>,
    #[serde(default)]
    pub workflows: Option<SystemWorkflowsConfig>,
    #[serde(default)]
    pub overlays: Option<SystemOverlaysConfig>,
    #[serde(default)]
    pub triage: Option<SystemTriageConfig>,
    #[serde(default)]
    pub repo_discovery: Option<SystemRepoDiscoveryConfig>,
    #[serde(default)]
    pub agents: Vec<SystemAgentConfig>,
    #[serde(default)]
    pub local_agent_defaults: Option<SystemLocalAgentDefaultsConfig>,
}

impl SystemConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read system config {}: {e}", path.display()))?;
        let wrapped: SystemTomlFile = toml::from_str(&raw)
            .map_err(|e| format!("failed to parse system config {}: {e}", path.display()))?;
        Ok(wrapped.system)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemTaskProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub url: String,
    pub project_id: u64,
    pub poll_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemGithubConfig {
    pub webhook_secret_env: String,
    pub poll_interval_secs: u64,
    pub owner_login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemLlmProviderConfig {
    pub name: String,
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemImagesConfig {
    #[serde(default)]
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemNetworkPoliciesConfig {
    #[serde(default)]
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemEnvironmentsConfig {
    #[serde(default)]
    pub installed: Vec<String>,
    pub triage_environment: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemWorkflowsConfig {
    #[serde(default)]
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemOverlaysConfig {
    pub registry: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemTriageConfig {
    /// Path (relative to the config root) of the triage prompt manifest.
    /// The manifest is a TOML file with a `[prompt]` section that defines
    /// the triage agent's system + user prompt. See `agent-hub-configs/triage.toml`.
    pub config: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemRepoDiscoveryConfig {
    pub enabled: bool,
    #[serde(default)]
    pub strategies: Vec<SystemRepoDiscoveryStrategyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemRepoDiscoveryStrategyConfig {
    #[serde(rename = "type")]
    pub strategy_type: String,
    pub path: String,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemAgentConfig {
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub supported_modes: Vec<String>,
    #[serde(default)]
    pub environments: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemLocalAgentDefaultsConfig {
    pub mirror_dir: String,
    pub worktree_dir: String,
    pub transcript_dir: String,
    pub transcript_retention_days: u64,
    pub fetch_interval_secs: u64,
}

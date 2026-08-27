use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemTomlFile {
    system: SystemConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfig {
    pub operator: String,
    pub bot_identity: String,
    #[serde(default)]
    pub task_provider: Option<SystemTaskProviderConfig>,
    #[serde(default)]
    pub github: Option<SystemGithubConfig>,
    #[serde(default)]
    pub scm_hosts: Vec<SystemScmHostConfig>,
    #[serde(default)]
    pub llm_providers: Vec<SystemLlmProviderConfig>,
    #[serde(default)]
    pub images: Option<SystemImagesConfig>,
    #[serde(default)]
    pub network_policies: Option<SystemNetworkPoliciesConfig>,
    #[serde(default)]
    pub environments: Option<SystemEnvironmentsConfig>,
    #[serde(default)]
    pub approval_bundle_trust: BTreeMap<String, SystemApprovalBundleTrustConfig>,
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SystemApprovalBundleTrustConfig {
    pub manifest_sha256: String,
    pub gateway_commit: String,
    pub gateway_archive_sha256: String,
    pub plugin_sha256: String,
    pub dippy_commit: String,
    pub dippy_archive_sha256: String,
    pub dippy_wheel_sha256: String,
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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SystemScmHostConfig {
    pub kind: String,
    pub host: String,
    pub api_base_url: String,
    pub read_token_env: Option<String>,
}

impl SystemConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read system config {}: {e}", path.display()))?;
        Self::from_toml(&raw, &path.display().to_string())
    }

    pub fn from_toml(raw: &str, label: &str) -> Result<Self, String> {
        let wrapped: SystemTomlFile = toml::from_str(raw)
            .map_err(|e| format!("failed to parse system config {label}: {e}"))?;
        if let Some(github) = wrapped.system.github.as_ref()
            && github.webhook_secret_env != "AGENT_HUB_GITHUB_WEBHOOK_SECRET"
        {
            return Err(
                "system.github.webhook_secret_env must be AGENT_HUB_GITHUB_WEBHOOK_SECRET"
                    .to_string(),
            );
        }
        for (name, trust) in &wrapped.system.approval_bundle_trust {
            if name.is_empty()
                || name.contains('/')
                || name.contains('\\')
                || !is_lower_hex(&trust.manifest_sha256, 64)
                || !is_lower_hex(&trust.gateway_commit, 40)
                || !is_lower_hex(&trust.gateway_archive_sha256, 64)
                || !is_lower_hex(&trust.plugin_sha256, 64)
                || !is_lower_hex(&trust.dippy_commit, 40)
                || !is_lower_hex(&trust.dippy_archive_sha256, 64)
                || !is_lower_hex(&trust.dippy_wheel_sha256, 64)
            {
                return Err(format!(
                    "system.approval_bundle_trust.{name} contains an invalid strict identity"
                ));
            }
        }
        let mut scm_hosts = BTreeSet::new();
        for scm_host in &wrapped.system.scm_hosts {
            let host = protocol::canonical_github_host(&scm_host.host)
                .unwrap_or_else(|| scm_host.host.trim().to_ascii_lowercase());
            if !scm_hosts.insert(host.clone()) {
                return Err(format!("system.scm_hosts contains duplicate host '{host}'"));
            }
        }
        Ok(wrapped.system)
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

#[cfg(test)]
mod tests {
    use super::*;

    const TRUST: &str = r#"
[system.approval_bundle_trust.code-supervised]
manifest_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
gateway_commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
gateway_archive_sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
plugin_sha256 = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
dippy_commit = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
dippy_archive_sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
dippy_wheel_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

    fn system_toml(scm_hosts: &str) -> String {
        format!(
            r#"[system]
operator = "test-operator"
bot_identity = "test-bot"

{scm_hosts}
{TRUST}
"#
        )
    }

    #[test]
    fn scm_hosts_round_trip_with_approval_bundle_trust() {
        let system = SystemConfig::from_toml(
            &system_toml(
                r#"[[system.scm_hosts]]
kind = "github"
host = "github.com"
api_base_url = "https://api.github.com"
read_token_env = "GITHUB_READ_TOKEN"

[[system.scm_hosts]]
kind = "github"
host = "github.snooguts.net"
api_base_url = "https://github.snooguts.net/api/v3"
read_token_env = "GITHUB_ENTERPRISE_READ_TOKEN""#,
            ),
            "test system",
        )
        .expect("parse system config");

        assert_eq!(
            system.scm_hosts,
            vec![
                SystemScmHostConfig {
                    kind: "github".to_string(),
                    host: "github.com".to_string(),
                    api_base_url: "https://api.github.com".to_string(),
                    read_token_env: Some("GITHUB_READ_TOKEN".to_string()),
                },
                SystemScmHostConfig {
                    kind: "github".to_string(),
                    host: "github.snooguts.net".to_string(),
                    api_base_url: "https://github.snooguts.net/api/v3".to_string(),
                    read_token_env: Some("GITHUB_ENTERPRISE_READ_TOKEN".to_string()),
                },
            ]
        );
        assert!(system.approval_bundle_trust.contains_key("code-supervised"));
        for scm_host in &system.scm_hosts {
            let serialized = toml::to_string(scm_host).expect("serialize SCM host");
            let reparsed: SystemScmHostConfig =
                toml::from_str(&serialized).expect("reparse serialized SCM host");
            assert_eq!(&reparsed, scm_host);
        }
    }

    #[test]
    fn scm_host_rejects_unknown_fields() {
        let error = SystemConfig::from_toml(
            &system_toml(
                r#"[[system.scm_hosts]]
kind = "github"
host = "github.com"
api_base_url = "https://api.github.com"
read_token_env = "GITHUB_READ_TOKEN"
write_token_env = "GITHUB_WRITE_TOKEN""#,
            ),
            "test system",
        )
        .expect_err("unknown SCM host field must fail");

        assert!(error.contains("unknown field `write_token_env`"), "{error}");
    }

    #[test]
    fn scm_host_rejects_missing_required_fields() {
        let error = SystemConfig::from_toml(
            &system_toml(
                r#"[[system.scm_hosts]]
kind = "github"
host = "github.com"
read_token_env = "GITHUB_READ_TOKEN""#,
            ),
            "test system",
        )
        .expect_err("missing SCM API base URL must fail");

        assert!(error.contains("missing field `api_base_url`"), "{error}");
    }

    #[test]
    fn scm_hosts_reject_duplicate_hosts() {
        let error = SystemConfig::from_toml(
            &system_toml(
                r#"[[system.scm_hosts]]
kind = "github"
host = "GitHub.com"
api_base_url = "https://api.github.com"
read_token_env = "GITHUB_READ_TOKEN"

[[system.scm_hosts]]
kind = "github"
host = "github.com"
api_base_url = "https://api.github.com"
read_token_env = "OTHER_GITHUB_READ_TOKEN""#,
            ),
            "test system",
        )
        .expect_err("duplicate SCM hosts must fail");

        assert_eq!(
            error,
            "system.scm_hosts contains duplicate host 'github.com'"
        );
    }
}

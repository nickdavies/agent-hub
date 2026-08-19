use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) use agent_hub_config::{
    ManagedRunnerFileMount, ManagedRunnerKind, ManagedRunnerSpec, OpencodeRunnerConfig,
};
use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::credential_provider::CredentialProvider;
use crate::mcp_bridge::McpBridgeConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAgentConfig {
    #[serde(default)]
    pub egress: EgressConfig,
    #[serde(default)]
    pub credential_providers: HashMap<String, CredentialProvider>,
    #[serde(default = "default_managed_agent_runner")]
    pub default_agent: ManagedRunnerSpec,
    /// Optional name of an entry in `managed_runners` to use as the default agent
    /// when a job is dispatched with `JobExecutionKind::Agent`. Takes precedence
    /// over the inline `default_agent` struct when set and resolvable.
    #[serde(default)]
    pub default_agent_id: Option<String>,
    #[serde(default)]
    pub managed_runners: HashMap<String, ManagedRunnerSpec>,
    #[serde(default)]
    pub mcp_bridges: Vec<McpBridgeConfig>,
    #[serde(default)]
    pub triage_repos: Vec<String>,
    #[serde(default)]
    pub approved_repository_roots: Vec<PathBuf>,
    /// Hosts the local-agent treats as "GitHub-shaped" when parsing repo URLs
    /// for repository identity and managed GitHub actions.
    /// Empty (or missing) means just `github.com`. Add GitHub Enterprise
    /// hosts (e.g. `github.snooguts.net`) here to opt in.
    #[serde(default)]
    pub github_hosts: Vec<String>,
    #[serde(default)]
    pub github_api_base_urls: HashMap<String, String>,
    #[serde(skip)]
    config_tree: Option<ConfigTreeSnapshot>,
}

impl Default for LocalAgentConfig {
    fn default() -> Self {
        Self {
            egress: EgressConfig::default(),
            credential_providers: HashMap::new(),
            default_agent: default_managed_agent_runner(),
            default_agent_id: None,
            managed_runners: HashMap::new(),
            mcp_bridges: Vec::new(),
            triage_repos: Vec::new(),
            approved_repository_roots: Vec::new(),
            github_hosts: Vec::new(),
            github_api_base_urls: HashMap::new(),
            config_tree: None,
        }
    }
}

impl LocalAgentConfig {
    /// Resolve the effective default-agent spec, honoring (in order):
    /// 1. an explicit `override_id` (e.g. from a CLI flag or env var)
    /// 2. `self.default_agent_id` from this config file
    /// 3. the inline `self.default_agent` struct
    ///
    /// Returns the resolved spec and a label describing where it came from
    /// (useful for startup logging).
    pub fn resolve_default_agent<'a>(
        &'a self,
        override_id: Option<&str>,
    ) -> anyhow::Result<(&'a ManagedRunnerSpec, String)> {
        let candidates = [
            (
                override_id.map(str::trim).filter(|s| !s.is_empty()),
                "override",
            ),
            (
                self.default_agent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                "config:default_agent_id",
            ),
        ];

        for (candidate, source) in candidates {
            if let Some(name) = candidate {
                let spec = self.managed_runners.get(name).ok_or_else(|| {
                    anyhow::anyhow!("{source} names unavailable managed runner '{name}'")
                })?;
                return Ok((spec, format!("{source}={name}")));
            }
        }

        Ok((
            &self.default_agent,
            "config:default_agent (inline)".to_string(),
        ))
    }

    pub fn config_tree(&self) -> Option<&ConfigTreeSnapshot> {
        self.config_tree.as_ref()
    }

    /// Returns the effective allowlist of GitHub-shaped hosts. Falls back to
    /// just `github.com` when `github_hosts` is missing or empty so existing
    /// configs keep working without modification. Entries are trimmed and
    /// blank ones are dropped; duplicates are *not* deduplicated (callers
    /// match case-insensitively, so this is harmless).
    pub fn github_host_allowlist(&self) -> Vec<String> {
        let trimmed: Vec<String> = self
            .github_hosts
            .iter()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .collect();
        if trimmed.is_empty() {
            vec![crate::artifacts::GITHUB_DEFAULT_HOST.to_string()]
        } else {
            trimmed
        }
    }

    pub fn network_policy(&self, policy_name: &str) -> Option<&ConfiguredNetworkPolicy> {
        self.config_tree
            .as_ref()
            .and_then(|tree| tree.network_policies.get(policy_name))
    }

    pub fn network_policy_env(&self, policy_name: &str) -> Option<&HashMap<String, String>> {
        self.config_tree
            .as_ref()
            .and_then(|tree| tree.network_policies.get(policy_name))
            .map(|policy| &policy.env)
    }

    #[cfg(test)]
    pub(crate) fn set_config_tree_for_tests(&mut self, config_tree: ConfigTreeSnapshot) {
        self.config_tree = Some(config_tree);
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigTreeSnapshot {
    pub environments: Vec<ConfiguredEnvironment>,
    pub network_policies: HashMap<String, ConfiguredNetworkPolicy>,
    pub llm_provider_hosts: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredEnvironment {
    pub name: String,
    pub allowed_overlays: Vec<String>,
    pub network_policy: String,
    pub bundle_digests: std::collections::BTreeMap<String, String>,
    pub network_policy_bundle_digests:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredNetworkPolicy {
    pub hosts: Vec<String>,
    pub uses_llm_provider_hosts: bool,
    pub env: HashMap<String, String>,
    pub unrestricted: bool,
    pub docker_network: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EgressConfig {
    #[serde(default)]
    pub llm: LlmHostConfig,
    #[serde(default)]
    pub build: BuildHostConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub preflight_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmHostConfig {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, Vec<String>>,
}

impl LlmHostConfig {
    pub fn resolve_provider_hosts(
        &self,
        provider: Option<&str>,
    ) -> anyhow::Result<(String, Vec<String>)> {
        let provider_key = provider
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| {
                self.default_provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_ascii_lowercase())
            });

        let provider_key = provider_key.context("egress.llm.default_provider is required")?;
        let hosts = self
            .providers
            .get(&provider_key)
            .ok_or_else(|| anyhow::anyhow!("unknown LLM provider '{provider_key}'"))?;
        let hosts = canonical_hosts(hosts);
        if hosts.is_empty() {
            anyhow::bail!("LLM provider '{provider_key}' must configure at least one host");
        }
        Ok((provider_key, hosts))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildHostConfig {
    #[serde(default = "default_github_hosts")]
    pub github_hosts: Vec<String>,
    #[serde(default = "default_go_registry_hosts")]
    pub go_registry_hosts: Vec<String>,
    #[serde(default = "default_crates_registry_hosts")]
    pub crates_registry_hosts: Vec<String>,
}

impl Default for BuildHostConfig {
    fn default() -> Self {
        Self {
            github_hosts: default_github_hosts(),
            go_registry_hosts: default_go_registry_hosts(),
            crates_registry_hosts: default_crates_registry_hosts(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_image")]
    pub image: String,
    #[serde(default = "default_proxy_port")]
    pub listen_port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            image: default_proxy_image(),
            listen_port: default_proxy_port(),
        }
    }
}

pub fn load_local_agent_config(path_override: Option<&Path>) -> anyhow::Result<LocalAgentConfig> {
    let config_root = config_root_from_env()?;
    let authoritative_path = config_root.join("local-agent.json");
    let path = match path_override {
        Some(path) if path == authoritative_path => path.to_path_buf(),
        Some(path) => anyhow::bail!(
            "local-agent config override '{}' is not the authoritative CONFIG_ROOT path '{}'",
            path.display(),
            authoritative_path.display()
        ),
        None => authoritative_path,
    };

    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let mut parsed: LocalAgentConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    normalize_config(&mut parsed)?;
    parsed.config_tree = Some(
        load_config_tree_snapshot(&config_root, &parsed.egress.llm)
            .with_context(|| format!("load config tree from {}", config_root.display()))?,
    );
    Ok(parsed)
}

fn config_root_from_env() -> anyhow::Result<PathBuf> {
    let raw = std::env::var("CONFIG_ROOT")
        .context("CONFIG_ROOT is required for local-agent and must point to config tree")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("CONFIG_ROOT is required for local-agent and must not be empty");
    }
    Ok(PathBuf::from(trimmed))
}

#[derive(Debug, Deserialize)]
struct SystemManifestFile {
    system: SystemManifestToml,
}

#[derive(Debug, Deserialize)]
struct SystemManifestToml {
    #[serde(default)]
    llm_providers: Vec<LlmProviderToml>,
    environments: EnvironmentsToml,
    network_policies: NetworkPoliciesToml,
}

#[derive(Debug, Deserialize)]
struct LlmProviderToml {
    name: String,
    #[serde(default)]
    hosts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EnvironmentsToml {
    #[serde(default)]
    installed: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NetworkPoliciesToml {
    #[serde(default)]
    installed: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EnvironmentFile {
    environment: EnvironmentToml,
}

#[derive(Debug, Deserialize)]
struct EnvironmentToml {
    name: String,
    network_policy: String,
    #[serde(default)]
    allowed_overlays: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NetworkPolicyFile {
    policy: NetworkPolicyToml,
}

#[derive(Debug, Clone, Deserialize)]
struct NetworkPolicyToml {
    name: String,
    extends: Option<String>,
    docker_network: Option<String>,
    unrestricted: Option<bool>,
    #[serde(default)]
    allow: Vec<PolicyAllowEntryToml>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PolicyAllowEntryToml {
    #[serde(default)]
    hosts: Vec<String>,
    hosts_from: Option<String>,
}

fn load_config_tree_snapshot(
    root: &Path,
    local_llm: &LlmHostConfig,
) -> anyhow::Result<ConfigTreeSnapshot> {
    let system_path = root.join("system.toml");
    let system_file: SystemManifestFile = read_toml_file(&system_path, "system manifest")?;
    let system = system_file.system;

    let mut environments = Vec::new();
    for environment_name in &system.environments.installed {
        let environment_path = root
            .join("environments")
            .join(environment_name)
            .join("environment.toml");
        let environment_file: EnvironmentFile =
            read_toml_file(&environment_path, "environment manifest")?;
        let environment = environment_file.environment;
        environments.push(ConfiguredEnvironment {
            name: environment.name,
            allowed_overlays: environment.allowed_overlays,
            network_policy: environment.network_policy,
            bundle_digests: agent_hub_config::directory_content_digests(
                environment_path
                    .parent()
                    .expect("environment manifest has parent"),
            )
            .map_err(anyhow::Error::msg)?,
            network_policy_bundle_digests: std::collections::BTreeMap::new(),
        });
    }

    let llm_provider_hosts = validate_llm_provider_host_maps(&system.llm_providers, local_llm)?;

    let mut policies_by_name = HashMap::new();
    for policy_name in &system.network_policies.installed {
        let policy_path = root
            .join("network_policies")
            .join(policy_name)
            .join("policy.toml");
        let policy_file: NetworkPolicyFile =
            read_toml_file(&policy_path, "network policy manifest")?;
        policies_by_name.insert(policy_file.policy.name.clone(), policy_file.policy);
    }

    for environment in &mut environments {
        let mut policy_name = Some(environment.network_policy.clone());
        let mut visited = HashSet::new();
        while let Some(name) = policy_name {
            if !visited.insert(name.clone()) {
                anyhow::bail!("network policy inheritance cycle detected at '{name}'");
            }
            environment.network_policy_bundle_digests.insert(
                name.clone(),
                agent_hub_config::directory_content_digests(
                    &root.join("network_policies").join(&name),
                )
                .map_err(anyhow::Error::msg)?,
            );
            policy_name = policies_by_name
                .get(&name)
                .and_then(|policy| policy.extends.clone());
        }
    }

    let mut network_policies = HashMap::new();
    let mut resolution_cache = HashMap::new();
    for policy_name in &system.network_policies.installed {
        let policy = resolve_network_policy(
            policy_name,
            &policies_by_name,
            &mut resolution_cache,
            &mut HashSet::new(),
        )?;
        network_policies.insert(policy_name.to_string(), policy);
    }

    Ok(ConfigTreeSnapshot {
        environments,
        network_policies,
        llm_provider_hosts,
    })
}

fn system_llm_provider_hosts(
    providers: &[LlmProviderToml],
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut hosts_by_provider = HashMap::new();
    for provider in providers {
        let name = provider.name.trim().to_ascii_lowercase();
        if name.is_empty() {
            anyhow::bail!("system.llm_providers name must not be empty");
        }
        let hosts = canonical_hosts(&provider.hosts);
        if hosts.is_empty() {
            anyhow::bail!("system LLM provider '{name}' must configure at least one host");
        }
        if hosts_by_provider.insert(name.clone(), hosts).is_some() {
            anyhow::bail!("system.llm_providers contains duplicate provider '{name}'");
        }
    }
    Ok(hosts_by_provider)
}

fn validate_llm_provider_host_maps(
    system_providers: &[LlmProviderToml],
    local: &LlmHostConfig,
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let system = system_llm_provider_hosts(system_providers)?;
    let local_hosts = local
        .providers
        .iter()
        .map(|(provider, hosts)| (provider.trim().to_ascii_lowercase(), canonical_hosts(hosts)))
        .collect::<HashMap<_, _>>();
    if system != local_hosts {
        anyhow::bail!(
            "system and local-agent LLM provider host maps must exactly match: system={system:?} local={local_hosts:?}"
        );
    }
    Ok(system)
}

fn resolve_network_policy(
    policy_name: &str,
    policies_by_name: &HashMap<String, NetworkPolicyToml>,
    cache: &mut HashMap<String, ConfiguredNetworkPolicy>,
    resolving: &mut HashSet<String>,
) -> anyhow::Result<ConfiguredNetworkPolicy> {
    if let Some(cached) = cache.get(policy_name) {
        return Ok(cached.clone());
    }

    if !resolving.insert(policy_name.to_string()) {
        anyhow::bail!("network policy inheritance cycle detected at '{policy_name}'");
    }

    let policy = policies_by_name
        .get(policy_name)
        .ok_or_else(|| anyhow::anyhow!("network policy '{policy_name}' is not loaded"))?;

    let mut resolved = if let Some(parent_name) = policy.extends.as_deref() {
        resolve_network_policy(parent_name, policies_by_name, cache, resolving)?
    } else {
        ConfiguredNetworkPolicy::default()
    };

    if policy.unrestricted.unwrap_or(false) {
        resolved.unrestricted = true;
        resolved.hosts.clear();
        resolved.uses_llm_provider_hosts = false;
    }
    if let Some(docker_network) = &policy.docker_network {
        resolved.docker_network = Some(docker_network.clone());
    }

    for allow in &policy.allow {
        resolved.hosts.extend(allow.hosts.clone());
        match allow.hosts_from.as_deref() {
            Some("system.llm_providers") => resolved.uses_llm_provider_hosts = true,
            Some(reference) => anyhow::bail!("unsupported hosts_from reference '{reference}'"),
            None => {}
        }
    }
    resolved.hosts = canonical_hosts(&resolved.hosts);

    for (key, value) in &policy.env {
        resolved.env.insert(key.clone(), value.clone());
    }

    resolving.remove(policy_name);
    cache.insert(policy_name.to_string(), resolved.clone());
    Ok(resolved)
}

fn read_toml_file<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {label} at {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse {label} at {}", path.display()))
}

fn normalize_config(config: &mut LocalAgentConfig) -> anyhow::Result<()> {
    config.egress.preflight_image = Some(
        config
            .egress
            .preflight_image
            .as_deref()
            .map(str::trim)
            .filter(|image| !image.is_empty())
            .context("egress.preflight_image is required")?
            .to_string(),
    );
    config.github_hosts = config
        .github_hosts
        .iter()
        .map(|host| {
            protocol::canonical_github_host(host)
                .ok_or_else(|| anyhow::anyhow!("invalid configured GitHub host '{host}'"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    config.github_api_base_urls = config
        .github_api_base_urls
        .iter()
        .map(|(host, api_base_url)| {
            let host = protocol::canonical_github_host(host)
                .ok_or_else(|| anyhow::anyhow!("invalid GitHub API host mapping '{host}'"))?;
            let api_base_url = protocol::canonical_github_api_base_for_host(&host, api_base_url)
                .ok_or_else(|| anyhow::anyhow!("invalid GitHub API base URL for '{host}'"))?;
            Ok((host, api_base_url))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;
    config.egress.llm.default_provider = config
        .egress
        .llm
        .default_provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .map(str::to_ascii_lowercase);
    let mut providers = HashMap::new();
    for (provider, hosts) in &config.egress.llm.providers {
        let provider = provider.trim().to_ascii_lowercase();
        if provider.is_empty() {
            anyhow::bail!("egress.llm.providers name must not be empty");
        }
        if providers
            .insert(provider.clone(), canonical_hosts(hosts))
            .is_some()
        {
            anyhow::bail!("egress.llm.providers contains duplicate provider '{provider}'");
        }
    }
    config.egress.llm.providers = providers;
    config.egress.build.github_hosts = normalize_hosts(&config.egress.build.github_hosts);
    config.egress.build.go_registry_hosts = normalize_hosts(&config.egress.build.go_registry_hosts);
    config.egress.build.crates_registry_hosts =
        normalize_hosts(&config.egress.build.crates_registry_hosts);

    normalize_runner_spec(&mut config.default_agent);
    config.managed_runners = config
        .managed_runners
        .drain()
        .map(|(name, mut spec)| {
            normalize_runner_spec(&mut spec);
            (name.trim().to_string(), spec)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    config.triage_repos = config
        .triage_repos
        .iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    config.approved_repository_roots = config
        .approved_repository_roots
        .iter()
        .filter(|root| !root.as_os_str().is_empty())
        .cloned()
        .collect();
    Ok(())
}

fn normalize_runner_spec(spec: &mut ManagedRunnerSpec) {
    spec.command = spec.command.trim().to_string();
    spec.args = spec
        .args
        .iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect();
    if spec
        .container_image
        .as_deref()
        .is_some_and(|image| image.trim().is_empty())
    {
        spec.container_image = None;
    }
    spec.file_mounts = spec
        .file_mounts
        .iter()
        .filter_map(|mount| {
            let host_path = mount.host_path.trim();
            let container_path = mount.container_path.trim();
            if host_path.is_empty() || container_path.is_empty() {
                return None;
            }
            Some(ManagedRunnerFileMount {
                host_path: host_path.to_string(),
                container_path: container_path.to_string(),
                read_only: mount.read_only,
            })
        })
        .collect();
    if spec.opencode.max_validation_attempts == 0 {
        spec.opencode.max_validation_attempts = default_opencode_validation_attempts();
    }
    spec.requires_credentials = spec
        .requires_credentials
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
}

pub fn normalize_hosts(hosts: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for host in hosts {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }

        if host.contains("://") || host.contains('/') || host.contains(char::is_whitespace) {
            continue;
        }

        let normalized = if let Some(suffix) = host.strip_prefix("*.") {
            format!("*.{}", suffix.trim_start_matches('.'))
        } else {
            host.trim_start_matches('.').to_string()
        };
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }

    out
}

pub fn canonical_hosts(hosts: &[String]) -> Vec<String> {
    let mut hosts = normalize_hosts(hosts);
    hosts.sort();
    hosts.dedup();
    hosts
}

fn default_proxy_image() -> String {
    // Use pinned stable image digest to avoid beta/default-latest drift.
    "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029"
        .to_string()
}

fn default_managed_agent_runner() -> ManagedRunnerSpec {
    ManagedRunnerSpec {
        kind: ManagedRunnerKind::Opencode,
        opencode: OpencodeRunnerConfig::default(),
        command: "sh".to_string(),
        args: vec![
            "-lc".to_string(),
            "if [ -f \"$AGENT_PROMPT_USER_PATH\" ]; then opencode run < \"$AGENT_PROMPT_USER_PATH\"; else opencode run; fi".to_string(),
        ],
        container_image: None,
        file_mounts: Vec::new(),
        requires_credentials: Vec::new(),
    }
}

const fn default_proxy_port() -> u16 {
    3128
}

fn default_github_hosts() -> Vec<String> {
    vec![
        "github.com".to_string(),
        "api.github.com".to_string(),
        "objects.githubusercontent.com".to_string(),
        "codeload.github.com".to_string(),
        "raw.githubusercontent.com".to_string(),
    ]
}

fn default_go_registry_hosts() -> Vec<String> {
    vec![
        "proxy.golang.org".to_string(),
        "sum.golang.org".to_string(),
        "go.dev".to_string(),
    ]
}

fn default_crates_registry_hosts() -> Vec<String> {
    vec![
        "index.crates.io".to_string(),
        "static.crates.io".to_string(),
        "crates.io".to_string(),
    ]
}

const fn default_opencode_validation_attempts() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_default_runner_ids_fail_when_unavailable() {
        let mut config = LocalAgentConfig::default();
        assert!(config.resolve_default_agent(None).is_ok());
        config.default_agent_id = Some("missing-default".to_string());
        assert!(config.resolve_default_agent(None).is_err());
        config.default_agent_id = None;
        assert!(
            config
                .resolve_default_agent(Some("missing-override"))
                .is_err()
        );
    }

    #[test]
    fn llm_hosts_resolve_from_provider_or_default() {
        let cfg = LlmHostConfig {
            default_provider: Some("openai".to_string()),
            providers: HashMap::from([
                (
                    "openai".to_string(),
                    vec!["api.openai.com".to_string(), "api.openai.com".to_string()],
                ),
                (
                    "anthropic".to_string(),
                    vec!["api.anthropic.com".to_string()],
                ),
            ]),
        };

        assert_eq!(
            cfg.resolve_provider_hosts(Some("anthropic")).unwrap().1,
            vec!["api.anthropic.com"]
        );
        assert_eq!(
            cfg.resolve_provider_hosts(None).unwrap().1,
            vec!["api.openai.com"]
        );
        assert!(cfg.resolve_provider_hosts(Some("missing")).is_err());
    }

    #[test]
    fn provider_host_maps_preserve_providers_and_compare_as_sets() {
        let providers = vec![
            LlmProviderToml {
                name: "first".to_string(),
                hosts: vec![
                    "login.example.com".to_string(),
                    "chat.example.com".to_string(),
                ],
            },
            LlmProviderToml {
                name: "second".to_string(),
                hosts: vec!["other.example.com".to_string()],
            },
        ];
        let mut local = LlmHostConfig {
            default_provider: Some("first".to_string()),
            providers: HashMap::from([
                (
                    "first".to_string(),
                    vec![
                        "chat.example.com".to_string(),
                        "login.example.com".to_string(),
                    ],
                ),
                ("second".to_string(), vec!["other.example.com".to_string()]),
            ]),
        };
        assert_eq!(
            validate_llm_provider_host_maps(&providers, &local).unwrap(),
            HashMap::from([
                (
                    "first".to_string(),
                    vec![
                        "chat.example.com".to_string(),
                        "login.example.com".to_string(),
                    ],
                ),
                ("second".to_string(), vec!["other.example.com".to_string()],),
            ])
        );

        local
            .providers
            .get_mut("first")
            .unwrap()
            .push("other.example.com".to_string());
        assert!(validate_llm_provider_host_maps(&providers, &local).is_err());
    }

    #[test]
    fn policy_inheritance_preserves_dynamic_provider_hosts() {
        let policies = HashMap::from([
            (
                "llm-only".to_string(),
                NetworkPolicyToml {
                    name: "llm-only".to_string(),
                    extends: None,
                    docker_network: None,
                    unrestricted: None,
                    allow: vec![PolicyAllowEntryToml {
                        hosts: Vec::new(),
                        hosts_from: Some("system.llm_providers".to_string()),
                    }],
                    env: HashMap::new(),
                },
            ),
            (
                "build".to_string(),
                NetworkPolicyToml {
                    name: "build".to_string(),
                    extends: Some("llm-only".to_string()),
                    docker_network: None,
                    unrestricted: None,
                    allow: vec![PolicyAllowEntryToml {
                        hosts: vec!["z.example.com".to_string(), "a.example.com".to_string()],
                        hosts_from: None,
                    }],
                    env: HashMap::new(),
                },
            ),
        ]);

        let policy =
            resolve_network_policy("build", &policies, &mut HashMap::new(), &mut HashSet::new())
                .unwrap();
        assert!(policy.uses_llm_provider_hosts);
        assert_eq!(policy.hosts, vec!["a.example.com", "z.example.com"]);
    }

    #[test]
    fn local_agent_config_requires_a_preflight_image() {
        let mut config = LocalAgentConfig::default();
        assert!(normalize_config(&mut config).is_err());

        config.egress.preflight_image = Some(" job-code:locked ".to_string());
        normalize_config(&mut config).unwrap();
        assert_eq!(
            config.egress.preflight_image.as_deref(),
            Some("job-code:locked")
        );
    }

    #[test]
    fn normalize_hosts_skips_invalid_values() {
        let hosts = normalize_hosts(&[
            "api.openai.com".to_string(),
            "https://api.openai.com".to_string(),
            " api.openai.com ".to_string(),
            "*.github.com".to_string(),
            ".objects.githubusercontent.com".to_string(),
            "/tmp/sock".to_string(),
        ]);

        assert_eq!(
            hosts,
            vec![
                "api.openai.com",
                "*.github.com",
                "objects.githubusercontent.com"
            ]
        );
    }

    #[test]
    fn normalize_runner_spec_strips_blank_fields() {
        let mut config = LocalAgentConfig {
            egress: EgressConfig {
                preflight_image: Some("job-code:locked".to_string()),
                ..EgressConfig::default()
            },
            credential_providers: HashMap::new(),
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Generic,
                opencode: OpencodeRunnerConfig::default(),
                command: " sh ".to_string(),
                args: vec![" -lc ".to_string(), " ".to_string()],
                container_image: Some(" ".to_string()),
                file_mounts: vec![ManagedRunnerFileMount {
                    host_path: " ".to_string(),
                    container_path: "/x".to_string(),
                    read_only: true,
                }],
                requires_credentials: Vec::new(),
            },
            default_agent_id: None,
            managed_runners: HashMap::from([(
                " deploy ".to_string(),
                ManagedRunnerSpec {
                    kind: ManagedRunnerKind::Generic,
                    opencode: OpencodeRunnerConfig::default(),
                    command: " runner ".to_string(),
                    args: vec!["--go".to_string()],
                    container_image: None,
                    file_mounts: vec![ManagedRunnerFileMount {
                        host_path: "/a ".to_string(),
                        container_path: " /b".to_string(),
                        read_only: false,
                    }],
                    requires_credentials: Vec::new(),
                },
            )]),
            mcp_bridges: Vec::new(),
            triage_repos: vec![
                " https://github.com/reddit/agent-hub.git ".to_string(),
                " ".to_string(),
            ],
            approved_repository_roots: Vec::new(),
            github_hosts: Vec::new(),
            github_api_base_urls: HashMap::new(),
            config_tree: None,
        };

        normalize_config(&mut config).expect("normalize config");
        assert_eq!(config.default_agent.command, "sh");
        assert_eq!(config.default_agent.args, vec!["-lc"]);
        assert!(config.default_agent.container_image.is_none());
        assert!(config.default_agent.file_mounts.is_empty());
        assert_eq!(config.default_agent.kind, ManagedRunnerKind::Generic);
        assert_eq!(config.default_agent.opencode.max_validation_attempts, 3);
        assert!(config.managed_runners.contains_key("deploy"));
        assert_eq!(
            config.triage_repos,
            vec!["https://github.com/reddit/agent-hub.git"]
        );
        assert_eq!(
            config.managed_runners["deploy"].file_mounts[0].host_path,
            "/a"
        );
        assert_eq!(
            config.managed_runners["deploy"].file_mounts[0].container_path,
            "/b"
        );
    }

    #[test]
    fn default_managed_runner_uses_opencode_run_non_interactive() {
        let runner = default_managed_agent_runner();

        assert_eq!(runner.kind, ManagedRunnerKind::Opencode);
        assert_eq!(runner.opencode.max_validation_attempts, 3);
        assert_eq!(runner.command, "sh");
        assert_eq!(runner.args[0], "-lc");
        assert!(runner.args[1].contains("opencode run"));
        assert!(runner.args[1].contains("$AGENT_PROMPT_USER_PATH"));
        assert!(runner.container_image.is_none());
    }
}

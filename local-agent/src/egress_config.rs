use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) use agent_hub_config::{
    ManagedRunnerFileMount, ManagedRunnerKind, ManagedRunnerSpec, OpencodeRunnerConfig,
};
use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::mcp_bridge::McpBridgeConfig;
use agent_hub_executor::credential_provider::CredentialProvider;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAgentConfig {
    #[serde(default = "default_installation_id")]
    pub installation_id: String,
    #[serde(default = "default_config_identity")]
    pub config_identity: String,
    #[serde(default)]
    pub egress: EgressConfig,
    #[serde(default)]
    pub credential_providers: HashMap<String, CredentialProvider>,
    #[serde(default, rename = "host_approvals")]
    _host_approvals: agent_hub_config::HostApprovalsConfig,
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
    pub root_registry: agent_hub_config::RootRegistryConfig,
    #[serde(default)]
    pub triage_mounts: Vec<ManagedRunnerFileMount>,
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
    #[serde(skip)]
    canonical_runner_profiles: Option<agent_hub_config::RunnerProfilesConfig>,
}

impl Default for LocalAgentConfig {
    fn default() -> Self {
        Self {
            installation_id: default_installation_id(),
            config_identity: default_config_identity(),
            egress: EgressConfig::default(),
            credential_providers: HashMap::new(),
            _host_approvals: agent_hub_config::HostApprovalsConfig::default(),
            default_agent: default_managed_agent_runner(),
            default_agent_id: None,
            managed_runners: HashMap::new(),
            mcp_bridges: Vec::new(),
            triage_repos: Vec::new(),
            root_registry: agent_hub_config::RootRegistryConfig::default(),
            triage_mounts: Vec::new(),
            github_hosts: Vec::new(),
            github_api_base_urls: HashMap::new(),
            config_tree: None,
            canonical_runner_profiles: None,
        }
    }
}

fn default_installation_id() -> String {
    "local-agent-default".to_string()
}

fn default_config_identity() -> String {
    "durable-opencode-default".to_string()
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
        if let Some(override_id) = override_id
            && (override_id.is_empty() || override_id.trim() != override_id)
        {
            anyhow::bail!(
                "default-agent override must be non-empty without surrounding whitespace"
            );
        }
        let default_agent = self
            .canonical_runner_profiles
            .as_ref()
            .map_or(&self.default_agent, |profiles| &profiles.default_agent);
        let default_agent_id = self
            .canonical_runner_profiles
            .as_ref()
            .and_then(|profiles| profiles.default_agent_id.as_deref())
            .or(self.default_agent_id.as_deref());
        let managed_runners = self
            .canonical_runner_profiles
            .as_ref()
            .map_or(&self.managed_runners, |profiles| &profiles.managed_runners);
        let candidates = [
            (override_id, "override"),
            (default_agent_id, "config:default_agent_id"),
        ];

        for (candidate, source) in candidates {
            if let Some(name) = candidate {
                let spec = managed_runners.get(name).ok_or_else(|| {
                    anyhow::anyhow!("{source} names unavailable managed runner '{name}'")
                })?;
                return Ok((spec, format!("{source}={name}")));
            }
        }

        Ok((default_agent, "config:default_agent (inline)".to_string()))
    }

    pub fn config_tree(&self) -> Option<&ConfigTreeSnapshot> {
        self.config_tree.as_ref()
    }

    pub fn canonical_runner_profiles(&self) -> Option<&agent_hub_config::RunnerProfilesConfig> {
        self.canonical_runner_profiles.as_ref()
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
    pub triage_mounts: Vec<agent_hub_config::TriageRepositoryMount>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredEnvironment {
    pub name: String,
    pub allowed_overlays: Vec<String>,
    pub network_policy: String,
    pub bundle_digest: String,
    pub bundle: agent_hub_config::EnvironmentBundle,
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
    pub resolver: ResolverConfig,
    #[serde(default)]
    pub preflight_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverConfig {
    #[serde(default = "default_resolver_upstream")]
    pub upstream: String,
    #[serde(default = "default_resolver_readiness_host")]
    pub readiness_host: String,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            upstream: default_resolver_upstream(),
            readiness_host: default_resolver_readiness_host(),
        }
    }
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
        let hosts = canonical_hosts(hosts)?;
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
    #[serde(default = "default_pypi_registry_hosts")]
    pub pypi_registry_hosts: Vec<String>,
    #[serde(default = "default_npm_registry_hosts")]
    pub npm_registry_hosts: Vec<String>,
}

impl Default for BuildHostConfig {
    fn default() -> Self {
        Self {
            github_hosts: default_github_hosts(),
            go_registry_hosts: default_go_registry_hosts(),
            crates_registry_hosts: default_crates_registry_hosts(),
            pypi_registry_hosts: default_pypi_registry_hosts(),
            npm_registry_hosts: default_npm_registry_hosts(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_image")]
    pub image: String,
    #[serde(default = "default_proxy_port")]
    pub listen_port: u16,
    #[serde(default = "default_proxy_runtime_uid")]
    pub runtime_uid: u32,
    #[serde(default = "default_proxy_runtime_gid")]
    pub runtime_gid: u32,
    #[serde(default = "default_proxy_writable_tmpfs_mode")]
    pub writable_tmpfs_mode: String,
    #[serde(default = "default_proxy_declared_volumes")]
    pub declared_volumes: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            image: default_proxy_image(),
            listen_port: default_proxy_port(),
            runtime_uid: default_proxy_runtime_uid(),
            runtime_gid: default_proxy_runtime_gid(),
            writable_tmpfs_mode: default_proxy_writable_tmpfs_mode(),
            declared_volumes: default_proxy_declared_volumes(),
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

    let deployment = agent_hub_config::validate_authoritative_config_root(&config_root)
        .map_err(anyhow::Error::msg)?;
    local_agent_config_from_validated_deployment(&config_root, &path, deployment)
}

pub(crate) fn local_agent_config_from_validated_deployment(
    config_root: &Path,
    path: &Path,
    deployment: agent_hub_config::ValidatedDeployment,
) -> anyhow::Result<LocalAgentConfig> {
    let mut parsed: LocalAgentConfig = serde_json::from_value(deployment.local_agent.clone())
        .with_context(|| format!("parse validated snapshot {}", path.display()))?;
    let selected = deployment
        .runner_profiles
        .resolve_agent_profile(None)
        .map_err(anyhow::Error::msg)?;
    parsed.default_agent = deployment.runner_profiles.default_agent.clone();
    parsed.default_agent_id = deployment.runner_profiles.default_agent_id.clone();
    parsed.managed_runners = deployment.runner_profiles.managed_runners.clone();
    parsed.egress = serde_json::from_value(selected.egress.clone())
        .context("parse canonical runner egress snapshot")?;
    parsed.github_hosts = deployment.runner_profiles.github_hosts.clone();
    normalize_config(&mut parsed)?;
    parsed.config_tree = Some(
        load_config_tree_snapshot(
            &deployment.system_toml,
            &deployment.environment_bundles,
            &deployment.network_policies,
            &deployment.triage_mounts,
            &parsed.egress.llm,
            &parsed.egress.build,
        )
        .with_context(|| format!("load config tree from {}", config_root.display()))?,
    );
    if serde_json::to_value(&parsed.egress)? != selected.egress {
        anyhow::bail!("operational egress does not match canonical authorized profile");
    }
    parsed.canonical_runner_profiles = Some(deployment.runner_profiles);
    Ok(parsed)
}

#[cfg(test)]
pub(crate) fn load_synthetic_runner_config_for_test(
    bytes: &[u8],
) -> anyhow::Result<LocalAgentConfig> {
    let authoritative_runner_profiles: agent_hub_config::RunnerProfilesConfig =
        serde_json::from_slice(bytes)?;
    let authoritative_runner_profiles = authoritative_runner_profiles
        .validated()
        .map_err(anyhow::Error::msg)?;
    let mut parsed: LocalAgentConfig = serde_json::from_slice(bytes)?;
    let selected = authoritative_runner_profiles
        .resolve_agent_profile(None)
        .map_err(anyhow::Error::msg)?;
    parsed.default_agent = authoritative_runner_profiles.default_agent.clone();
    parsed.default_agent_id = authoritative_runner_profiles.default_agent_id.clone();
    parsed.managed_runners = authoritative_runner_profiles.managed_runners.clone();
    parsed.egress = serde_json::from_value(selected.egress)?;
    parsed.github_hosts = authoritative_runner_profiles.github_hosts.clone();
    normalize_config(&mut parsed)?;
    parsed.canonical_runner_profiles = Some(authoritative_runner_profiles);
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

type NetworkPolicyToml = agent_hub_config::NetworkPolicyConfig;

fn load_config_tree_snapshot(
    system_toml: &str,
    bundles: &std::collections::BTreeMap<String, agent_hub_config::EnvironmentBundle>,
    policy_snapshots: &std::collections::BTreeMap<String, agent_hub_config::ValidatedNetworkPolicy>,
    triage_mounts: &[agent_hub_config::TriageRepositoryMount],
    local_llm: &LlmHostConfig,
    local_build: &BuildHostConfig,
) -> anyhow::Result<ConfigTreeSnapshot> {
    let system_file: SystemManifestFile =
        toml::from_str(system_toml).context("parse captured validated system manifest")?;
    let system = system_file.system;

    let mut environments = Vec::new();
    for environment_name in &system.environments.installed {
        let bundle = bundles
            .get(environment_name)
            .with_context(|| format!("validated bundle '{environment_name}' is unavailable"))?;
        let environment_bytes = bundle
            .files
            .iter()
            .find(|file| file.path == "environment.toml")
            .with_context(|| format!("bundle '{environment_name}' has no environment.toml"))?;
        let environment_file: EnvironmentFile = toml::from_str(
            std::str::from_utf8(&environment_bytes.bytes)
                .context("environment manifest is not UTF-8")?,
        )
        .context("parse captured environment manifest")?;
        let environment = environment_file.environment;
        environments.push(ConfiguredEnvironment {
            name: environment.name,
            allowed_overlays: environment.allowed_overlays,
            network_policy: environment.network_policy,
            bundle_digest: bundle.digest.clone(),
            bundle: bundle.clone(),
            network_policy_bundle_digests: std::collections::BTreeMap::new(),
        });
    }

    let llm_provider_hosts = validate_llm_provider_host_maps(&system.llm_providers, local_llm)?;

    let policies_by_name = system
        .network_policies
        .installed
        .iter()
        .map(|name| {
            policy_snapshots
                .get(name)
                .map(|snapshot| (name.clone(), snapshot.policy.clone()))
                .with_context(|| format!("validated network policy '{name}' is unavailable"))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;

    for environment in &mut environments {
        let mut policy_name = Some(environment.network_policy.clone());
        let mut visited = HashSet::new();
        while let Some(name) = policy_name {
            if !visited.insert(name.clone()) {
                anyhow::bail!("network policy inheritance cycle detected at '{name}'");
            }
            environment.network_policy_bundle_digests.insert(
                name.clone(),
                policy_snapshots
                    .get(&name)
                    .with_context(|| {
                        format!("validated digest for policy '{name}' is unavailable")
                    })?
                    .file_digests
                    .clone(),
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
    validate_typed_resource_policies(&network_policies, local_llm, local_build)?;

    Ok(ConfigTreeSnapshot {
        environments,
        network_policies,
        llm_provider_hosts,
        triage_mounts: triage_mounts.to_vec(),
    })
}

fn validate_typed_resource_policies(
    policies: &HashMap<String, ConfiguredNetworkPolicy>,
    llm: &LlmHostConfig,
    build: &BuildHostConfig,
) -> anyhow::Result<()> {
    if !policies.contains_key("guarded-web") {
        return Ok(());
    }
    let expected = [
        ("llm-only", Vec::new(), true),
        ("github-only", canonical_hosts(&build.github_hosts)?, false),
        (
            "go-registry",
            canonical_hosts(&build.go_registry_hosts)?,
            false,
        ),
        (
            "crates-registry",
            canonical_hosts(&build.crates_registry_hosts)?,
            false,
        ),
        (
            "pypi-registry",
            canonical_hosts(&build.pypi_registry_hosts)?,
            false,
        ),
        (
            "npm-registry",
            canonical_hosts(&build.npm_registry_hosts)?,
            false,
        ),
        ("guarded-web", Vec::new(), false),
    ];
    for (name, hosts, dynamic_llm) in expected {
        let policy = policies
            .get(name)
            .with_context(|| format!("typed network policy '{name}' is required"))?;
        if policy.hosts != hosts || policy.uses_llm_provider_hosts != dynamic_llm {
            anyhow::bail!("typed network policy '{name}' does not match its resource contract");
        }
        if policy.unrestricted || policy.docker_network.is_some() {
            anyhow::bail!("typed network policy '{name}' must remain restricted");
        }
    }
    llm.resolve_provider_hosts(None)?;
    let build_policy = policies
        .get("build")
        .context("typed network policy 'build' is required")?;
    let mut build_hosts = Vec::new();
    for name in [
        "github-only",
        "go-registry",
        "crates-registry",
        "pypi-registry",
        "npm-registry",
    ] {
        build_hosts.extend(policies[name].hosts.clone());
    }
    if build_policy.hosts != canonical_hosts(&build_hosts)?
        || !build_policy.uses_llm_provider_hosts
        || build_policy.unrestricted
        || build_policy.docker_network.is_some()
    {
        anyhow::bail!("typed network policy 'build' must exactly union build resource policies");
    }
    Ok(())
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
        let hosts = canonical_hosts(&provider.hosts)?;
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
        .map(|(provider, hosts)| {
            Ok((
                provider.trim().to_ascii_lowercase(),
                canonical_hosts(hosts)?,
            ))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;
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

    if policy.unrestricted {
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
    resolved.hosts = canonical_hosts(&resolved.hosts)?;

    for (key, value) in &policy.env {
        resolved.env.insert(key.clone(), value.clone());
    }

    resolving.remove(policy_name);
    cache.insert(policy_name.to_string(), resolved.clone());
    Ok(resolved)
}

fn normalize_config(config: &mut LocalAgentConfig) -> anyhow::Result<()> {
    config.installation_id = config.installation_id.trim().to_string();
    if config.installation_id.is_empty()
        || !config
            .installation_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("installation_id must be a non-empty label-safe identifier");
    }
    config.config_identity = config.config_identity.trim().to_string();
    if config.config_identity.is_empty()
        || !config
            .config_identity
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        anyhow::bail!("config_identity must be a non-empty label-safe identifier");
    }
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
            .insert(provider.clone(), canonical_hosts(hosts)?)
            .is_some()
        {
            anyhow::bail!("egress.llm.providers contains duplicate provider '{provider}'");
        }
    }
    config.egress.llm.providers = providers;
    config.egress.build.github_hosts = canonical_hosts(&config.egress.build.github_hosts)?;
    config.egress.build.go_registry_hosts =
        canonical_hosts(&config.egress.build.go_registry_hosts)?;
    config.egress.build.crates_registry_hosts =
        canonical_hosts(&config.egress.build.crates_registry_hosts)?;
    config.egress.build.pypi_registry_hosts =
        canonical_hosts(&config.egress.build.pypi_registry_hosts)?;
    config.egress.build.npm_registry_hosts =
        canonical_hosts(&config.egress.build.npm_registry_hosts)?;
    if config.egress.proxy.image.trim() != config.egress.proxy.image
        || config.egress.proxy.image.is_empty()
    {
        anyhow::bail!("egress.proxy.image must be non-empty without surrounding whitespace");
    }
    if config.egress.proxy.listen_port == 0 {
        anyhow::bail!("egress.proxy.listen_port must be non-zero");
    }
    let (resolver_address, resolver_port) = config
        .egress
        .resolver
        .upstream
        .rsplit_once('@')
        .context("egress.resolver.upstream must be an IP literal with @port")?;
    resolver_address
        .parse::<std::net::IpAddr>()
        .context("egress.resolver.upstream address is invalid")?;
    if resolver_port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .is_none()
    {
        anyhow::bail!("egress.resolver.upstream port is invalid");
    }
    canonical_hosts(std::slice::from_ref(&config.egress.resolver.readiness_host))?;
    if config.egress.proxy.writable_tmpfs_mode.len() != 4
        || !config.egress.proxy.writable_tmpfs_mode.starts_with('0')
        || !config
            .egress
            .proxy
            .writable_tmpfs_mode
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'7'))
    {
        anyhow::bail!("egress.proxy.writable_tmpfs_mode must be a four-digit octal string");
    }
    config.egress.proxy.declared_volumes.sort();
    if config.egress.proxy.declared_volumes
        != ["/var/log/squid".to_string(), "/var/spool/squid".to_string()]
    {
        anyhow::bail!(
            "egress.proxy.declared_volumes must contain exactly /var/log/squid and /var/spool/squid"
        );
    }

    config.triage_repos = config
        .triage_repos
        .iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect();
    agent_hub_config::root_registry(&serde_json::json!({"root_registry": config.root_registry}))
        .map_err(anyhow::Error::msg)?;
    for mount in &mut config.triage_mounts {
        mount.host_path = mount.host_path.trim().to_string();
        mount.container_path = mount.container_path.trim().to_string();
        if mount.host_path.is_empty() || mount.container_path.is_empty() || !mount.read_only {
            anyhow::bail!("triage_mounts must contain non-empty read-only source/target pairs");
        }
    }
    let mut bridge_names = HashSet::new();
    for bridge in &config.mcp_bridges {
        let name = bridge.name.trim();
        if name.is_empty() {
            anyhow::bail!("MCP bridge name must not be empty");
        }
        if !bridge_names.insert(name.to_string()) {
            anyhow::bail!("duplicate MCP bridge name '{name}'");
        }
    }
    Ok(())
}

pub fn canonical_hosts(hosts: &[String]) -> anyhow::Result<Vec<String>> {
    agent_hub_config::canonical_policy_hosts(hosts).map_err(anyhow::Error::msg)
}

fn default_proxy_image() -> String {
    // Use pinned stable image digest to avoid beta/default-latest drift.
    "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029"
        .to_string()
}

fn default_resolver_upstream() -> String {
    "127.0.0.11@53".to_string()
}

fn default_resolver_readiness_host() -> String {
    "example.com".to_string()
}

fn default_managed_agent_runner() -> ManagedRunnerSpec {
    ManagedRunnerSpec {
        kind: ManagedRunnerKind::Opencode,
        opencode: OpencodeRunnerConfig::default(),
        command: "/usr/bin/sh".to_string(),
        args: vec![
            "-lc".to_string(),
            "if [ -f \"$AGENT_PROMPT_USER_PATH\" ]; then /usr/local/bin/opencode run < \"$AGENT_PROMPT_USER_PATH\"; else /usr/local/bin/opencode run; fi".to_string(),
        ],
        container_image: None,
        file_mounts: Vec::new(),
        requires_credentials: Vec::new(),
    }
}

const fn default_proxy_port() -> u16 {
    3128
}

const fn default_proxy_runtime_uid() -> u32 {
    13
}

const fn default_proxy_runtime_gid() -> u32 {
    13
}

fn default_proxy_writable_tmpfs_mode() -> String {
    "0755".to_string()
}

fn default_proxy_declared_volumes() -> Vec<String> {
    vec!["/var/log/squid".to_string(), "/var/spool/squid".to_string()]
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

fn default_pypi_registry_hosts() -> Vec<String> {
    vec!["files.pythonhosted.org".to_string(), "pypi.org".to_string()]
}

fn default_npm_registry_hosts() -> Vec<String> {
    vec!["registry.npmjs.org".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_bridge::{McpBridgeConfig, McpEndpoint};

    fn add_root_registry(config: &mut LocalAgentConfig) {
        config.root_registry = agent_hub_config::RootRegistryConfig {
            roots: vec![agent_hub_config::RootConfig {
                identity: "test".into(),
                path: "/roots/test".into(),
                roles: vec![agent_hub_config::RootRole::WorkspaceRo],
            }],
        };
    }

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
    fn local_config_rejects_duplicate_mcp_bridge_names() {
        let bridge = McpBridgeConfig {
            name: "duplicate".to_string(),
            host_endpoint: McpEndpoint::Unix {
                path: "/tmp/source.sock".to_string(),
            },
            container_socket_path: None,
        };
        let mut config = LocalAgentConfig {
            installation_id: default_installation_id(),
            config_identity: default_config_identity(),
            triage_mounts: Vec::new(),
            canonical_runner_profiles: None,
            egress: EgressConfig {
                preflight_image: Some("test".to_string()),
                ..EgressConfig::default()
            },
            mcp_bridges: vec![bridge.clone(), bridge],
            ..LocalAgentConfig::default()
        };
        assert!(normalize_config(&mut config).is_err());
    }

    #[test]
    fn proxy_declared_volume_contract_requires_exact_log_and_spool_set() {
        let mut valid = LocalAgentConfig::default();
        add_root_registry(&mut valid);
        valid.egress.preflight_image = Some("test".to_string());
        normalize_config(&mut valid).unwrap();
        assert_eq!(
            valid.egress.proxy.declared_volumes,
            ["/var/log/squid", "/var/spool/squid"]
        );

        for volumes in [
            vec!["/var/log/squid".to_string()],
            vec![
                "/var/log/squid".to_string(),
                "/var/spool/squid".to_string(),
                "/extra".to_string(),
            ],
        ] {
            let mut invalid = LocalAgentConfig::default();
            add_root_registry(&mut invalid);
            invalid.egress.preflight_image = Some("test".to_string());
            invalid.egress.proxy.declared_volumes = volumes;
            assert!(normalize_config(&mut invalid).is_err());
        }
    }

    #[test]
    fn captured_validated_runner_snapshot_survives_local_agent_file_replacement() {
        let root =
            std::env::temp_dir().join(format!("validated-runner-toctou-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("system.toml"),
            "[system]\noperator='test'\nbot_identity='test'\n[system.environments]\ninstalled=[]\ntriage_environment='none'\n[system.network_policies]\ninstalled=[]\n",
        )
        .unwrap();
        let local_path = root.join("local-agent.json");
        let local = serde_json::json!({
            "installation_id":"test-installation",
            "config_identity":"test-config",
            "root_registry":{"roots":[{"identity":"test","path":"/roots/test","roles":["workspace_ro"]}]},
            "egress":{"preflight_image":"test-preflight"},
            "default_agent":{"kind":"generic","command":"true"}
        });
        std::fs::write(&local_path, serde_json::to_vec(&local).unwrap()).unwrap();
        let deployment = agent_hub_config::validate_authoritative_config_root(&root).unwrap();

        let mut replacement = local;
        replacement["default_agent"]["command"] = serde_json::json!("false");
        std::fs::write(&local_path, serde_json::to_vec(&replacement).unwrap()).unwrap();

        let captured =
            local_agent_config_from_validated_deployment(&root, &local_path, deployment).unwrap();
        assert_eq!(
            captured.resolve_default_agent(None).unwrap().0.command,
            "true"
        );
        assert_eq!(
            agent_hub_config::validate_authoritative_config_root(&root)
                .unwrap()
                .runner_profiles
                .resolve_agent_profile(None)
                .unwrap()
                .command,
            "false"
        );
        std::fs::remove_dir_all(root).unwrap();
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
                    description: "llm".to_string(),
                    extends: None,
                    docker_network: None,
                    unrestricted: false,
                    allow: vec![agent_hub_config::PolicyAllowRule {
                        description: None,
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
                    description: "build".to_string(),
                    extends: Some("llm-only".to_string()),
                    docker_network: None,
                    unrestricted: false,
                    allow: vec![agent_hub_config::PolicyAllowRule {
                        description: None,
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
        add_root_registry(&mut config);
        assert!(normalize_config(&mut config).is_err());

        config.egress.preflight_image = Some(" job-code:locked ".to_string());
        normalize_config(&mut config).unwrap();
        assert_eq!(
            config.egress.preflight_image.as_deref(),
            Some("job-code:locked")
        );
    }

    #[test]
    fn canonical_hosts_rejects_invalid_values_instead_of_dropping_them() {
        assert!(canonical_hosts(&["https://api.openai.com".to_string()]).is_err());
        assert!(canonical_hosts(&["  api.openai.com".to_string()]).is_err());
        assert!(canonical_hosts(&["127.0.0.1".to_string()]).is_err());
        assert_eq!(
            canonical_hosts(&["Z.example.com".to_string(), "a.example.com".to_string(),]).unwrap(),
            ["a.example.com", "z.example.com"]
        );
    }

    #[test]
    fn default_managed_runner_uses_opencode_run_non_interactive() {
        let runner = default_managed_agent_runner();

        assert_eq!(runner.kind, ManagedRunnerKind::Opencode);
        assert_eq!(runner.opencode.max_validation_attempts, 3);
        assert_eq!(runner.command, "/usr/bin/sh");
        assert_eq!(runner.args[0], "-lc");
        assert!(runner.args[1].contains("/usr/local/bin/opencode run"));
        assert!(runner.args[1].contains("$AGENT_PROMPT_USER_PATH"));
        assert!(runner.container_image.is_none());
    }
}

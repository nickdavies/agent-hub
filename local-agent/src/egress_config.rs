use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::credential_provider::CredentialProvider;
use crate::mcp_bridge::McpBridgeConfig;

#[derive(Debug, Clone, Deserialize)]
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
    /// for `AGENT_REPO_SLUG`, `GH_HOST`, and `gh auth token` resolution.
    /// Empty (or missing) means just `github.com`. Add GitHub Enterprise
    /// hosts (e.g. `github.snooguts.net`) here to opt in.
    #[serde(default)]
    pub github_hosts: Vec<String>,
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
            config_tree: None,
        }
    }
}

impl LocalAgentConfig {
    pub fn managed_runner_for_operation(&self, operation: &str) -> Option<&ManagedRunnerSpec> {
        let key = operation.trim();
        if key.is_empty() {
            return None;
        }
        self.managed_runners.get(key)
    }

    /// Resolve the effective default-agent spec, honoring (in order):
    /// 1. an explicit `override_id` (e.g. from a CLI flag or env var)
    /// 2. `self.default_agent_id` from this config file
    /// 3. the inline `self.default_agent` struct (legacy fallback)
    ///
    /// Returns the resolved spec and a label describing where it came from
    /// (useful for startup logging).
    pub fn resolve_default_agent<'a>(
        &'a self,
        override_id: Option<&str>,
    ) -> (&'a ManagedRunnerSpec, String) {
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
            if let Some(name) = candidate
                && let Some(spec) = self.managed_runners.get(name)
            {
                return (spec, format!("{source}={name}"));
            }
        }

        (
            &self.default_agent,
            "config:default_agent (inline)".to_string(),
        )
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
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredEnvironment {
    pub name: String,
    pub allowed_overlays: Vec<String>,
    /// Runtime preference for this environment. Values are operator-defined
    /// names that map to `managed_runners` keys (e.g. "cursor-default" or
    /// "opencode-default"), or the special string "auto" / `None` to defer to
    /// the local-agent's effective default.
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfiguredNetworkPolicy {
    pub hosts: Vec<String>,
    pub env: HashMap<String, String>,
    pub unrestricted: bool,
    pub docker_network: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManagedRunnerSpec {
    #[serde(default)]
    pub kind: ManagedRunnerKind,
    #[serde(default)]
    pub opencode: OpencodeRunnerConfig,
    #[serde(default)]
    pub cursor: CursorRunnerConfig,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub file_mounts: Vec<ManagedRunnerFileMount>,
    /// Names of credential providers (from `credential_providers`) that must be
    /// resolved and injected before this runner can launch. Useful for
    /// frontend-specific API keys (e.g. `CURSOR_API_KEY`).
    #[serde(default)]
    pub requires_credentials: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunnerKind {
    #[default]
    Generic,
    Opencode,
    Cursor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpencodeRunnerConfig {
    #[serde(default = "default_opencode_validation_attempts")]
    pub max_validation_attempts: u32,
}

impl Default for OpencodeRunnerConfig {
    fn default() -> Self {
        Self {
            max_validation_attempts: default_opencode_validation_attempts(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CursorRunnerConfig {
    #[serde(default = "default_cursor_validation_attempts")]
    pub max_validation_attempts: u32,
    /// Default cursor model id (e.g. `auto`, `sonnet-4.5-thinking`). When unset,
    /// the runner does not pass `--model` and cursor picks based on its own
    /// defaults.
    #[serde(default)]
    pub default_model: Option<String>,
}

impl Default for CursorRunnerConfig {
    fn default() -> Self {
        Self {
            max_validation_attempts: default_cursor_validation_attempts(),
            default_model: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManagedRunnerFileMount {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LlmHostConfig {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, Vec<String>>,
}

impl LlmHostConfig {
    pub fn hosts_for_provider(&self, provider: Option<&str>) -> Vec<String> {
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

        let mut out = Vec::new();
        let Some(provider_key) = provider_key else {
            return out;
        };

        if let Some(hosts) = self.providers.get(&provider_key) {
            out.extend(normalize_hosts(hosts));
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
    let path = match path_override {
        Some(path) => path.to_path_buf(),
        None => default_local_agent_config_path()?,
    };

    let config_root = config_root_from_env()?;

    if !path.exists() {
        let mut config = LocalAgentConfig::default();
        config.config_tree = Some(
            load_config_tree_snapshot(&config_root)
                .with_context(|| format!("load config tree from {}", config_root.display()))?,
        );
        return Ok(config);
    }

    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let mut parsed: LocalAgentConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    normalize_config(&mut parsed);
    parsed.config_tree = Some(
        load_config_tree_snapshot(&config_root)
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
    #[serde(default)]
    allowed_overlays: Vec<String>,
    #[serde(default)]
    runtime: Option<String>,
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

fn load_config_tree_snapshot(root: &Path) -> anyhow::Result<ConfigTreeSnapshot> {
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
        environments.push(ConfiguredEnvironment {
            name: environment_file.environment.name,
            allowed_overlays: environment_file.environment.allowed_overlays,
            runtime: environment_file
                .environment
                .runtime
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        });
    }

    let llm_hosts = normalize_hosts(
        &system
            .llm_providers
            .into_iter()
            .flat_map(|provider| provider.hosts)
            .collect::<Vec<_>>(),
    );

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

    let mut network_policies = HashMap::new();
    let mut resolution_cache = HashMap::new();
    for policy_name in &system.network_policies.installed {
        let policy = resolve_network_policy(
            policy_name,
            &policies_by_name,
            &llm_hosts,
            &mut resolution_cache,
            &mut HashSet::new(),
        )?;
        network_policies.insert(policy_name.to_string(), policy);
    }

    Ok(ConfigTreeSnapshot {
        environments,
        network_policies,
    })
}

fn resolve_network_policy(
    policy_name: &str,
    policies_by_name: &HashMap<String, NetworkPolicyToml>,
    llm_hosts: &[String],
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
        resolve_network_policy(parent_name, policies_by_name, llm_hosts, cache, resolving)?
    } else {
        ConfiguredNetworkPolicy::default()
    };

    if policy.unrestricted.unwrap_or(false) {
        resolved.unrestricted = true;
        resolved.hosts.clear();
    }
    if let Some(docker_network) = &policy.docker_network {
        resolved.docker_network = Some(docker_network.clone());
    }

    for allow in &policy.allow {
        resolved.hosts.extend(allow.hosts.clone());
        if allow.hosts_from.as_deref() == Some("system.llm_providers") {
            resolved.hosts.extend(llm_hosts.iter().cloned());
        }
    }
    resolved.hosts = normalize_hosts(&resolved.hosts);

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

fn normalize_config(config: &mut LocalAgentConfig) {
    config.egress.llm.providers = config
        .egress
        .llm
        .providers
        .iter()
        .map(|(provider, hosts)| (provider.to_ascii_lowercase(), normalize_hosts(hosts)))
        .collect();
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
    if spec.cursor.max_validation_attempts == 0 {
        spec.cursor.max_validation_attempts = default_cursor_validation_attempts();
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

fn default_local_agent_config_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; pass --local-agent-config")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("agent-hub")
        .join("local-agent.json"))
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
        cursor: CursorRunnerConfig::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            cfg.hosts_for_provider(Some("anthropic")),
            vec!["api.anthropic.com"]
        );
        assert_eq!(cfg.hosts_for_provider(None), vec!["api.openai.com"]);
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
            egress: EgressConfig::default(),
            credential_providers: HashMap::new(),
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Generic,
                opencode: OpencodeRunnerConfig::default(),
                cursor: CursorRunnerConfig::default(),
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
                    cursor: CursorRunnerConfig::default(),
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
            config_tree: None,
        };

        normalize_config(&mut config);
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

const fn default_opencode_validation_attempts() -> u32 {
    3
}

const fn default_cursor_validation_attempts() -> u32 {
    3
}

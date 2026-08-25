use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const OPENCODE_PROJECT_CONFIG_ALIASES: &[&str] = &[
    "opencode.json",
    "opencode.jsonc",
    ".opencode/opencode.json",
    ".opencode/opencode.jsonc",
];
pub const DURABLE_OPENCODE_PATH: &str =
    "/usr/local/go/bin:/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerProfilesConfig {
    #[serde(default = "default_managed_agent_runner")]
    pub default_agent: ManagedRunnerSpec,
    #[serde(default)]
    pub default_agent_id: Option<String>,
    #[serde(default)]
    pub managed_runners: HashMap<String, ManagedRunnerSpec>,
    #[serde(default)]
    pub credential_providers: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub mcp_bridges: Vec<serde_json::Value>,
    #[serde(default = "default_egress_value")]
    pub egress: serde_json::Value,
    #[serde(default)]
    pub github_hosts: Vec<String>,
}

impl Default for RunnerProfilesConfig {
    fn default() -> Self {
        Self {
            default_agent: default_managed_agent_runner(),
            default_agent_id: None,
            managed_runners: HashMap::new(),
            credential_providers: HashMap::new(),
            mcp_bridges: Vec::new(),
            egress: default_egress_value(),
            github_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ManagedRunnerSpec {
    #[serde(default)]
    pub kind: ManagedRunnerKind,
    #[serde(default)]
    pub opencode: OpencodeRunnerConfig,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub file_mounts: Vec<ManagedRunnerFileMount>,
    #[serde(default)]
    pub requires_credentials: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunnerKind {
    #[default]
    Generic,
    Opencode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpencodeRunnerConfig {
    #[serde(default = "default_opencode_validation_attempts")]
    pub max_validation_attempts: u32,
    #[serde(default = "trusted_opencode_runtime_env")]
    pub trusted_env: BTreeMap<String, String>,
}

impl Default for OpencodeRunnerConfig {
    fn default() -> Self {
        Self {
            max_validation_attempts: default_opencode_validation_attempts(),
            trusted_env: trusted_opencode_runtime_env(),
        }
    }
}

pub fn trusted_opencode_runtime_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".to_string(), "/opencode/state/home".to_string()),
        (
            "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS".to_string(),
            "1".to_string(),
        ),
        (
            "OPENCODE_DISABLE_EXTERNAL_SKILLS".to_string(),
            "1".to_string(),
        ),
        ("OPENCODE_DISABLE_MODELS_FETCH".to_string(), "1".to_string()),
        (
            "OPENCODE_DISABLE_PROJECT_CONFIG".to_string(),
            "1".to_string(),
        ),
        ("OPENCODE_PURE".to_string(), "1".to_string()),
        (
            "OPENCODE_EXPERIMENTAL_WEBSOCKETS".to_string(),
            "0".to_string(),
        ),
        ("PATH".to_string(), DURABLE_OPENCODE_PATH.to_string()),
        ("XDG_CACHE_HOME".to_string(), "/opencode/cache".to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            "/opencode/config".to_string(),
        ),
        ("XDG_DATA_HOME".to_string(), "/opencode/data".to_string()),
        ("XDG_STATE_HOME".to_string(), "/opencode/state".to_string()),
    ])
}

pub fn validate_untrusted_opencode_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    for name in env.keys() {
        validate_untrusted_opencode_env_name(name)?;
    }
    Ok(())
}

pub fn validate_untrusted_opencode_env_name(name: &str) -> Result<(), String> {
    const EXACT_DENY: &[&str] = &[
        "PATH",
        "HOME",
        "AWKLIBPATH",
        "AWKPATH",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "BASH_ENV",
        "BASHOPTS",
        "CARGO_HOME",
        "CDPATH",
        "CLAUDE_CONFIG_DIR",
        "CLASSPATH",
        "COREPACK_HOME",
        "ENV",
        "FPATH",
        "GCONV_PATH",
        "GEM_HOME",
        "GEM_PATH",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_EXEC_PATH",
        "GIT_SSH_COMMAND",
        "GOENV",
        "GOROOT",
        "GOTOOLDIR",
        "GOWORK",
        "IFS",
        "INI_SCAN_DIR",
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "KSH_ENV",
        "LIBPATH",
        "LOCPATH",
        "LUA_CPATH",
        "LUA_INIT",
        "LUA_PATH",
        "NLSPATH",
        "NODE_OPTIONS",
        "NODE_PATH",
        "PHPRC",
        "POSIXLY_CORRECT",
        "PROMPT_COMMAND",
        "PS4",
        "PYTHONBREAKPOINT",
        "PYTHONHOME",
        "PYTHONINSPECT",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "PYTHONUSERBASE",
        "PYTHONWARNINGS",
        "PERL5DB",
        "PERL5LIB",
        "PERL5OPT",
        "PERL5SHELL",
        "PERLLIB",
        "RUBYLIB",
        "RUBYOPT",
        "RUBYGEMS_GEMDEPS",
        "SHELL",
        "SHELLOPTS",
        "SHLIB_PATH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTUP_HOME",
        "TEMP",
        "TMP",
        "TMPDIR",
        "TCLLIBPATH",
        "GLIBC_TUNABLES",
        "MUSL_LOCPATH",
        "ZDOTDIR",
        "ZSH_ENV",
        "_JAVA_OPTIONS",
    ];
    const RUNTIME_PREFIX_DENY: &[&str] = &[
        "BASH_FUNC_",
        "BUN_",
        "BUNDLE_",
        "DENO_",
        "DYLD_",
        "LD_",
        "NPM_CONFIG_",
        "PNPM_",
        "YARN_",
    ];
    const PROVIDER_PREFIX_DENY: &[&str] = &[
        "ANTHROPIC_",
        "AWS_",
        "AZURE_",
        "CEREBRAS_",
        "CLOUDFLARE_",
        "COHERE_",
        "DATABRICKS_",
        "DEEPSEEK_",
        "FIREWORKS_",
        "GEMINI_",
        "GOOGLE_",
        "GROQ_",
        "HF_",
        "HUGGINGFACE_",
        "LMSTUDIO_",
        "MISTRAL_",
        "NVIDIA_",
        "OCI_",
        "OLLAMA_",
        "OPENAI_",
        "OPENROUTER_",
        "PERPLEXITY_",
        "REPLICATE_",
        "SAMBANOVA_",
        "TOGETHER_",
        "VERTEX_",
        "XAI_",
    ];
    const CREDENTIAL_SUFFIX_DENY: &[&str] = &[
        "_API_KEY",
        "_AUTH_TOKEN",
        "_ACCESS_TOKEN",
        "_API_TOKEN",
        "_SECRET_ACCESS_KEY",
        "_TOKEN",
    ];

    let denied = name.starts_with("OPENCODE_")
        || EXACT_DENY.contains(&name)
        || PROVIDER_PREFIX_DENY
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || RUNTIME_PREFIX_DENY
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || CREDENTIAL_SUFFIX_DENY
            .iter()
            .any(|suffix| name.ends_with(suffix));
    if denied {
        Err(format!(
            "untrusted OpenCode environment variable '{name}' is not allowed"
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ManagedRunnerFileMount {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialProviderDescriptor {
    pub name: String,
    pub provider_type: String,
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedRunnerProfile {
    pub profile_name: String,
    pub kind: ManagedRunnerKind,
    pub command: String,
    pub args: Vec<String>,
    pub container_image: Option<String>,
    pub file_mounts: Vec<ManagedRunnerFileMount>,
    pub credential_providers: Vec<CredentialProviderDescriptor>,
    #[serde(default)]
    pub trusted_mounts: Vec<TrustedMountDescriptor>,
    pub mcp_bridges: Vec<McpBridgeDescriptor>,
    pub egress: serde_json::Value,
    pub github_hosts: Vec<String>,
    pub opencode: OpencodeRunnerConfig,
    pub config_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrustedMountDescriptor {
    pub purpose: String,
    pub container_path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpBridgeDescriptor {
    pub name: String,
    pub endpoint_type: String,
    pub target: String,
    pub container_socket_path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
}

impl RunnerProfilesConfig {
    pub fn validated(mut self) -> Result<Self, String> {
        validate_optional_identifier(self.default_agent_id.as_deref(), "default_agent_id")?;
        validate_runner_spec(&mut self.default_agent, "default_agent")?;
        let mut normalized_runners = HashMap::new();
        for (name, mut spec) in self.managed_runners {
            validate_identifier(&name, "managed runner name")?;
            validate_runner_spec(&mut spec, &name)?;
            if normalized_runners.insert(name.clone(), spec).is_some() {
                return Err(format!("duplicate managed runner '{name}'"));
            }
        }
        self.managed_runners = normalized_runners;
        self.github_hosts = canonical_unique_strings(&self.github_hosts, "github_hosts", true)?;
        self.egress = canonicalize_egress(self.egress)?;
        for (name, definition) in &self.credential_providers {
            validate_identifier(name, "credential provider name")?;
            let object = definition
                .as_object()
                .ok_or_else(|| format!("credential provider '{name}' must be an object"))?;
            let provider_type = definition
                .get("type")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("credential provider '{name}' has no type"))?;
            validate_identifier(provider_type, "credential provider type")?;
            if let Some(declared_name) = object.get("name").and_then(serde_json::Value::as_str) {
                validate_identifier(declared_name, "credential provider declared name")?;
                if declared_name != name {
                    return Err(format!(
                        "credential provider key '{name}' does not match declared name '{declared_name}'"
                    ));
                }
            }
            for key in [
                "host_auth_path",
                "container_path",
                "capture_file",
                "capture_env",
            ] {
                if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
                    validate_exact_nonempty(value, &format!("credential provider '{name}' {key}"))?;
                }
            }
        }
        let mut bridge_names = BTreeSet::new();
        for bridge in &self.mcp_bridges {
            let descriptor = mcp_bridge_descriptor(bridge)?;
            let object = bridge
                .as_object()
                .ok_or_else(|| "MCP bridge must be an object".to_string())?;
            validate_identifier(&descriptor.name, "MCP bridge name")?;
            if object.get("name").and_then(serde_json::Value::as_str)
                != Some(descriptor.name.as_str())
            {
                return Err(format!(
                    "MCP bridge '{}' name is noncanonical",
                    descriptor.name
                ));
            }
            validate_exact_nonempty(&descriptor.container_socket_path, "MCP container socket")?;
            validate_exact_nonempty(&descriptor.target, "MCP endpoint target")?;
            for argument in &descriptor.args {
                validate_exact_nonempty(argument, "MCP endpoint argument")?;
            }
            let mut env_keys = BTreeSet::new();
            for key in &descriptor.env_keys {
                validate_identifier(key, "MCP environment key")?;
                if !env_keys.insert(key) {
                    return Err(format!(
                        "MCP bridge '{}' has duplicate environment key",
                        descriptor.name
                    ));
                }
            }
            if !bridge_names.insert(descriptor.name.clone()) {
                return Err(format!("duplicate MCP bridge name '{}'", descriptor.name));
            }
        }
        self.resolve_agent_profile(None)?;
        let mut names = self.managed_runners.keys().cloned().collect::<Vec<_>>();
        names.sort();
        for name in names {
            self.resolve_profile(name.clone(), &self.managed_runners[&name])?;
        }
        Ok(self)
    }

    pub fn resolve_agent_profile(
        &self,
        requested_name: Option<&str>,
    ) -> Result<ResolvedRunnerProfile, String> {
        if let Some(name) = requested_name
            && name != "auto"
        {
            validate_identifier(name, "requested runner profile")?;
        }
        let requested = requested_name
            .filter(|value| *value != "auto")
            .or(self.default_agent_id.as_deref());
        let (name, spec) = match requested {
            Some(name) => (
                name.to_string(),
                self.managed_runners
                    .get(name)
                    .ok_or_else(|| format!("unknown managed runner profile '{name}'"))?,
            ),
            None => ("default_agent".to_string(), &self.default_agent),
        };
        self.resolve_profile(name, spec)
    }

    pub fn resolve_managed_profile(
        &self,
        operation: &str,
    ) -> Result<ResolvedRunnerProfile, String> {
        validate_identifier(operation, "managed operation")?;
        let name = operation;
        let spec = self
            .managed_runners
            .get(name)
            .ok_or_else(|| format!("unknown managed runner profile '{name}'"))?;
        self.resolve_profile(name.to_string(), spec)
    }

    fn resolve_profile(
        &self,
        name: String,
        spec: &ManagedRunnerSpec,
    ) -> Result<ResolvedRunnerProfile, String> {
        let mut spec = spec.clone();
        validate_runner_spec(&mut spec, &name)?;
        if spec.command.is_empty() {
            return Err(format!("managed runner profile '{name}' has no command"));
        }
        if spec.kind == ManagedRunnerKind::Opencode && !spec.file_mounts.is_empty() {
            return Err(format!(
                "OpenCode runner profile '{name}' must not declare host file mounts"
            ));
        }
        let mut credential_providers = spec
            .requires_credentials
            .iter()
            .map(|provider_name| {
                let definition = self.credential_providers.get(provider_name).ok_or_else(|| {
                    format!(
                        "managed runner profile '{name}' requires unknown credential provider '{provider_name}'"
                    )
                })?;
                credential_descriptor(provider_name, definition)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for provider in &mut credential_providers {
            provider.targets.sort();
            provider.targets.dedup();
        }
        credential_providers.sort_by(|left, right| {
            (&left.name, &left.provider_type, &left.targets).cmp(&(
                &right.name,
                &right.provider_type,
                &right.targets,
            ))
        });
        let mut mcp_bridges = self
            .mcp_bridges
            .iter()
            .map(mcp_bridge_descriptor)
            .collect::<Result<Vec<_>, _>>()?;
        for bridge in &mut mcp_bridges {
            bridge.env_keys.sort();
            bridge.env_keys.dedup();
        }
        mcp_bridges.sort_by(mcp_bridge_sort_key);
        if mcp_bridges
            .windows(2)
            .any(|bridges| bridges[0].name == bridges[1].name)
        {
            return Err(format!(
                "managed runner profile '{name}' has duplicate MCP bridge names"
            ));
        }
        let mut github_hosts = self
            .github_hosts
            .iter()
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect::<Vec<_>>();
        github_hosts.sort();
        github_hosts.dedup();

        let profile = ResolvedRunnerProfile {
            profile_name: name,
            kind: spec.kind,
            command: spec.command,
            args: spec.args,
            container_image: spec.container_image,
            file_mounts: spec.file_mounts,
            credential_providers,
            trusted_mounts: trusted_mounts_for_kind(spec.kind),
            mcp_bridges,
            egress: canonical_set_json(merge_json_defaults(
                default_egress_value(),
                self.egress.clone(),
            )),
            github_hosts,
            opencode: spec.opencode,
            config_digest: String::new(),
        };
        canonicalize_resolved_runner_profile(&profile)
    }
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        other => other,
    }
}

fn canonical_set_json(value: serde_json::Value) -> serde_json::Value {
    match canonical_json(value) {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical_set_json(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            let mut values = values
                .into_iter()
                .map(canonical_set_json)
                .collect::<Vec<_>>();
            values.sort_by_key(ToString::to_string);
            values.dedup();
            serde_json::Value::Array(values)
        }
        other => other,
    }
}

fn merge_json_defaults(
    defaults: serde_json::Value,
    configured: serde_json::Value,
) -> serde_json::Value {
    match (defaults, configured) {
        (serde_json::Value::Object(mut defaults), serde_json::Value::Object(configured)) => {
            for (key, value) in configured {
                let merged = defaults
                    .remove(&key)
                    .map(|default| merge_json_defaults(default, value.clone()))
                    .unwrap_or(value);
                defaults.insert(key, merged);
            }
            serde_json::Value::Object(defaults)
        }
        (_, serde_json::Value::Null) => serde_json::Value::Null,
        (_, configured) => configured,
    }
}

fn default_egress_value() -> serde_json::Value {
    serde_json::json!({
        "llm": {"default_provider": null, "providers": {}},
        "build": {
            "github_hosts": [
                "github.com",
                "api.github.com",
                "codeload.github.com",
                "raw.githubusercontent.com",
                "objects.githubusercontent.com"
            ],
            "go_registry_hosts": [
                "proxy.golang.org",
                "sum.golang.org",
                "go.dev"
            ],
            "crates_registry_hosts": [
                "crates.io",
                "index.crates.io",
                "static.crates.io"
            ],
            "pypi_registry_hosts": [
                "files.pythonhosted.org",
                "pypi.org"
            ],
            "npm_registry_hosts": [
                "registry.npmjs.org"
            ]
        },
        "proxy": {
            "image": "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029",
            "listen_port": 3128,
            "runtime_uid": 13,
            "runtime_gid": 13,
            "writable_tmpfs_mode": "0755",
            "declared_volumes": ["/var/log/squid", "/var/spool/squid"]
        },
        "resolver": {"upstream": "127.0.0.11@53", "readiness_host": "example.com"},
        "preflight_image": null
    })
}

fn canonical_profile_without_digest(
    profile: &ResolvedRunnerProfile,
) -> Result<ResolvedRunnerProfile, String> {
    let mut canonical = profile.clone();
    canonical.config_digest.clear();
    canonical.file_mounts.sort_by(|left, right| {
        (&left.host_path, &left.container_path, left.read_only).cmp(&(
            &right.host_path,
            &right.container_path,
            right.read_only,
        ))
    });
    canonical.file_mounts.dedup();
    for provider in &mut canonical.credential_providers {
        provider.targets.sort();
        provider.targets.dedup();
    }
    canonical.credential_providers.sort_by(|left, right| {
        (&left.name, &left.provider_type, &left.targets).cmp(&(
            &right.name,
            &right.provider_type,
            &right.targets,
        ))
    });
    canonical.trusted_mounts.sort();
    canonical.trusted_mounts.dedup();
    for bridge in &mut canonical.mcp_bridges {
        bridge.env_keys.sort();
        bridge.env_keys.dedup();
    }
    canonical.mcp_bridges.sort_by(mcp_bridge_sort_key);
    canonical.github_hosts = canonical
        .github_hosts
        .iter()
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    canonical.egress = canonicalize_resolved_egress(canonical.egress)?;
    Ok(canonical)
}

pub fn canonicalize_resolved_runner_profile(
    profile: &ResolvedRunnerProfile,
) -> Result<ResolvedRunnerProfile, String> {
    let mut canonical = canonical_profile_without_digest(profile)?;
    let bytes = serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
    canonical.config_digest = hex::encode(Sha256::digest(bytes));
    Ok(canonical)
}

pub fn digest_profile(profile: &ResolvedRunnerProfile) -> Result<String, String> {
    let canonical = canonical_profile_without_digest(profile)?;
    let bytes = serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn canonicalize_resolved_egress(value: serde_json::Value) -> Result<serde_json::Value, String> {
    fn merge_set_values(left: &mut serde_json::Value, right: serde_json::Value) {
        if let (Some(left), Some(right)) = (left.as_array_mut(), right.as_array()) {
            left.extend(right.iter().cloned());
        }
    }

    fn walk(value: serde_json::Value, key: Option<&str>) -> Result<serde_json::Value, String> {
        match value {
            serde_json::Value::Object(object) => {
                let mut output = BTreeMap::<String, serde_json::Value>::new();
                for (raw_key, value) in object {
                    let canonical_key = if key == Some("providers") {
                        raw_key.trim().to_ascii_lowercase()
                    } else {
                        raw_key
                    };
                    let child_key = if key == Some("providers") {
                        "provider_hosts"
                    } else {
                        canonical_key.as_str()
                    };
                    let value = walk(value, Some(child_key))?;
                    if let Some(existing) = output.get_mut(&canonical_key) {
                        merge_set_values(existing, value);
                    } else {
                        output.insert(canonical_key, value);
                    }
                }
                if key == Some("providers") {
                    for value in output.values_mut() {
                        if let Some(values) = value.as_array_mut() {
                            values.sort_by_key(ToString::to_string);
                            values.dedup();
                        }
                    }
                }
                Ok(serde_json::Value::Object(output.into_iter().collect()))
            }
            serde_json::Value::Array(values)
                if key.is_some_and(|key| key.contains("hosts"))
                    || key == Some("declared_volumes") =>
            {
                let lowercase = key != Some("declared_volumes");
                let mut values = values
                    .into_iter()
                    .map(|value| {
                        let value = value
                            .as_str()
                            .ok_or_else(|| "egress set values must be strings".to_string())?
                            .trim()
                            .to_string();
                        Ok(if lowercase {
                            value.to_ascii_lowercase()
                        } else {
                            value
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                values.sort();
                values.dedup();
                Ok(serde_json::Value::Array(
                    values.into_iter().map(serde_json::Value::String).collect(),
                ))
            }
            serde_json::Value::Array(values) => Ok(serde_json::Value::Array(
                values
                    .into_iter()
                    .map(|value| walk(value, key))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            serde_json::Value::String(value) if key == Some("default_provider") => {
                Ok(serde_json::Value::String(value.trim().to_ascii_lowercase()))
            }
            other => Ok(other),
        }
    }

    canonicalize_egress(walk(value, None)?)
}

fn mcp_bridge_sort_key(
    left: &McpBridgeDescriptor,
    right: &McpBridgeDescriptor,
) -> std::cmp::Ordering {
    (
        &left.name,
        &left.endpoint_type,
        &left.target,
        &left.container_socket_path,
        &left.args,
        &left.env_keys,
    )
        .cmp(&(
            &right.name,
            &right.endpoint_type,
            &right.target,
            &right.container_socket_path,
            &right.args,
            &right.env_keys,
        ))
}

fn mcp_bridge_descriptor(value: &serde_json::Value) -> Result<McpBridgeDescriptor, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "MCP bridge must be an object".to_string())?;
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "MCP bridge has no name".to_string())?;
    let endpoint = object
        .get("host_endpoint")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("MCP bridge '{name}' has no endpoint"))?;
    let endpoint_type = endpoint
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("MCP bridge '{name}' endpoint has no type"))?;
    let (target, args, mut env_keys) = match endpoint_type {
        "unix" => (
            endpoint
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Vec::new(),
            Vec::new(),
        ),
        "tcp" => (
            format!(
                "{}:{}",
                endpoint
                    .get("host")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                endpoint
                    .get("port")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default()
            ),
            Vec::new(),
            Vec::new(),
        ),
        "stdio" => (
            endpoint
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            endpoint
                .get("args")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect(),
            endpoint
                .get("env")
                .and_then(serde_json::Value::as_object)
                .map(|env| env.keys().cloned().collect())
                .unwrap_or_default(),
        ),
        other => {
            return Err(format!(
                "MCP bridge '{name}' has unsupported endpoint '{other}'"
            ));
        }
    };
    env_keys.sort();
    Ok(McpBridgeDescriptor {
        name: name.to_string(),
        endpoint_type: endpoint_type.to_string(),
        target,
        container_socket_path: object
            .get("container_socket_path")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("/tmp/mcp-{name}.sock")),
        args,
        env_keys,
    })
}

pub fn directory_content_digests(root: &Path) -> Result<BTreeMap<String, String>, String> {
    fn visit(
        root: &Path,
        path: &Path,
        output: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        let mut entries = std::fs::read_dir(path)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            let entry_path = entry.path();
            if file_type.is_symlink() {
                return Err(format!(
                    "configuration bundle contains symlink {}",
                    entry_path.display()
                ));
            }
            if file_type.is_dir() {
                visit(root, &entry_path, output)?;
            } else if file_type.is_file() {
                let relative = entry_path
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                output.insert(
                    relative,
                    hex::encode(Sha256::digest(
                        std::fs::read(&entry_path).map_err(|error| error.to_string())?,
                    )),
                );
            }
        }
        Ok(())
    }

    let mut output = BTreeMap::new();
    visit(root, root, &mut output)?;
    Ok(output)
}

fn credential_descriptor(
    configured_name: &str,
    value: &serde_json::Value,
) -> Result<CredentialProviderDescriptor, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("credential provider '{configured_name}' must be an object"))?;
    let provider_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("credential provider '{configured_name}' has no type"))?;
    let mut targets = match provider_type {
        "env" => object
            .get("vars")
            .and_then(serde_json::Value::as_object)
            .map(|values| values.keys().cloned().collect())
            .unwrap_or_default(),
        "file" => string_target(object, "container_path"),
        "command" => ["capture_env", "capture_file"]
            .into_iter()
            .flat_map(|key| string_target(object, key))
            .collect(),
        "kubeconfig-filter" => string_target(object, "container_path"),
        "opencode-chatgpt-oauth" => vec!["/opencode/data/opencode/auth.json".to_string()],
        other => {
            return Err(format!(
                "credential provider '{configured_name}' has unsupported type '{other}'"
            ));
        }
    };
    targets.sort();
    targets.dedup();
    Ok(CredentialProviderDescriptor {
        name: configured_name.to_string(),
        provider_type: provider_type.to_string(),
        targets,
    })
}

fn trusted_mounts_for_kind(kind: ManagedRunnerKind) -> Vec<TrustedMountDescriptor> {
    if kind != ManagedRunnerKind::Opencode {
        return Vec::new();
    }
    vec![
        TrustedMountDescriptor {
            purpose: "environment_bundle".to_string(),
            container_path: "/opencode/config/opencode".to_string(),
            read_only: true,
        },
        TrustedMountDescriptor {
            purpose: "xdg_data".to_string(),
            container_path: "/opencode/data".to_string(),
            read_only: false,
        },
        TrustedMountDescriptor {
            purpose: "xdg_state".to_string(),
            container_path: "/opencode/state".to_string(),
            read_only: false,
        },
        TrustedMountDescriptor {
            purpose: "xdg_cache".to_string(),
            container_path: "/opencode/cache".to_string(),
            read_only: false,
        },
    ]
}

fn string_target(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Vec<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

fn validate_runner_spec(spec: &mut ManagedRunnerSpec, name: &str) -> Result<(), String> {
    validate_exact_nonempty(&spec.command, &format!("runner '{name}' command"))?;
    if let Some(image) = &spec.container_image {
        validate_exact_nonempty(image, &format!("runner '{name}' image"))?;
    }
    for (index, argument) in spec.args.iter().enumerate() {
        validate_exact_nonempty(argument, &format!("runner '{name}' argument {index}"))?;
        if argument.contains('\0') {
            return Err(format!("runner '{name}' argument {index} contains NUL"));
        }
    }
    for mount in &spec.file_mounts {
        validate_exact_nonempty(&mount.host_path, &format!("runner '{name}' mount source"))?;
        validate_exact_nonempty(
            &mount.container_path,
            &format!("runner '{name}' mount destination"),
        )?;
    }
    let mut credentials = BTreeSet::new();
    for credential in &spec.requires_credentials {
        validate_identifier(credential, &format!("runner '{name}' credential"))?;
        if !credentials.insert(credential.clone()) {
            return Err(format!(
                "runner '{name}' has duplicate credential '{credential}'"
            ));
        }
    }
    spec.requires_credentials = credentials.into_iter().collect();
    if spec.kind == ManagedRunnerKind::Opencode && spec.opencode.max_validation_attempts == 0 {
        return Err(format!(
            "runner '{name}' max_validation_attempts must be greater than zero"
        ));
    }
    if spec.kind == ManagedRunnerKind::Opencode
        && spec.opencode.trusted_env != trusted_opencode_runtime_env()
    {
        return Err(format!(
            "runner '{name}' trusted OpenCode environment is not canonical"
        ));
    }
    Ok(())
}

fn validate_exact_nonempty(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.trim() != value {
        return Err(format!(
            "{label} must be non-empty without surrounding whitespace"
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    validate_exact_nonempty(value, label)?;
    if value.chars().any(char::is_control) {
        return Err(format!("{label} contains control characters"));
    }
    Ok(())
}

fn validate_optional_identifier(value: Option<&str>, label: &str) -> Result<(), String> {
    if let Some(value) = value {
        validate_identifier(value, label)?;
    }
    Ok(())
}

fn canonical_unique_strings(
    values: &[String],
    label: &str,
    lowercase: bool,
) -> Result<Vec<String>, String> {
    let mut canonical = BTreeSet::new();
    for value in values {
        validate_exact_nonempty(value, label)?;
        let value = if lowercase {
            value.to_ascii_lowercase()
        } else {
            value.clone()
        };
        if !canonical.insert(value.clone()) {
            return Err(format!("{label} contains duplicate '{value}'"));
        }
    }
    Ok(canonical.into_iter().collect())
}

fn canonicalize_egress(value: serde_json::Value) -> Result<serde_json::Value, String> {
    fn walk(value: serde_json::Value, key: Option<&str>) -> Result<serde_json::Value, String> {
        match value {
            serde_json::Value::Object(object) => {
                let mut output = serde_json::Map::new();
                for (raw_key, value) in object {
                    let key_name = if key == Some("providers") {
                        let canonical = raw_key.to_ascii_lowercase();
                        validate_exact_nonempty(&raw_key, "egress provider name")?;
                        canonical
                    } else {
                        raw_key
                    };
                    let child_key = if key == Some("providers") {
                        "provider_hosts"
                    } else {
                        &key_name
                    };
                    if output
                        .insert(key_name.clone(), walk(value, Some(child_key))?)
                        .is_some()
                    {
                        return Err(format!(
                            "egress contains duplicate canonical key '{key_name}'"
                        ));
                    }
                }
                Ok(serde_json::Value::Object(output))
            }
            serde_json::Value::Array(values) => {
                if key.is_some_and(|key| key.contains("hosts")) || key == Some("providers") {
                    let strings = values
                        .into_iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .ok_or_else(|| "egress host lists must contain strings".to_string())
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(serde_json::Value::Array(
                        crate::network_policy::canonical_policy_hosts(&strings)?
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ));
                }
                Ok(serde_json::Value::Array(
                    values
                        .into_iter()
                        .map(|value| walk(value, key))
                        .collect::<Result<Vec<_>, _>>()?,
                ))
            }
            serde_json::Value::String(value) => {
                validate_exact_nonempty(&value, "egress string")?;
                Ok(serde_json::Value::String(
                    if key == Some("default_provider") {
                        value.to_ascii_lowercase()
                    } else {
                        value
                    },
                ))
            }
            other => Ok(other),
        }
    }
    let canonical = canonical_set_json(walk(value, None)?);
    if let Some(proxy) = canonical.get("proxy") {
        let proxy = proxy
            .as_object()
            .ok_or_else(|| "egress.proxy must be an object".to_string())?;
        for field in ["runtime_uid", "runtime_gid"] {
            if let Some(value) = proxy.get(field)
                && value
                    .as_u64()
                    .is_none_or(|value| value > u64::from(u32::MAX))
            {
                return Err(format!(
                    "egress.proxy.{field} must be an unsigned 32-bit integer"
                ));
            }
        }
        if let Some(value) = proxy.get("writable_tmpfs_mode") {
            let valid = value.as_str().is_some_and(|value| {
                value.len() == 4
                    && value.starts_with('0')
                    && value.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
            });
            if !valid {
                return Err(
                    "egress.proxy.writable_tmpfs_mode must be a four-digit octal string"
                        .to_string(),
                );
            }
        }
        if let Some(value) = proxy.get("declared_volumes") {
            let mut volumes = value
                .as_array()
                .ok_or_else(|| "egress.proxy.declared_volumes must be an array".to_string())?
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        "egress.proxy.declared_volumes entries must be strings".to_string()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            volumes.sort_unstable();
            if volumes != ["/var/log/squid", "/var/spool/squid"] {
                return Err(
                    "egress.proxy.declared_volumes must contain exactly /var/log/squid and /var/spool/squid"
                        .to_string(),
                );
            }
        }
    }
    if let Some(resolver) = canonical.get("resolver") {
        let upstream = resolver
            .get("upstream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "egress.resolver.upstream must be a string".to_string())?;
        let (address, port) = upstream
            .rsplit_once('@')
            .ok_or_else(|| "egress.resolver.upstream must include @port".to_string())?;
        address
            .parse::<std::net::IpAddr>()
            .map_err(|_| "egress.resolver.upstream address must be an IP literal".to_string())?;
        if port.parse::<u16>().ok().filter(|port| *port != 0).is_none() {
            return Err("egress.resolver.upstream port is invalid".to_string());
        }
        let readiness_host = resolver
            .get("readiness_host")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "egress.resolver.readiness_host must be a string".to_string())?;
        if !readiness_host.contains('.')
            || readiness_host
                .split('.')
                .any(|label| label.is_empty() || label.len() > 63)
        {
            return Err("egress.resolver.readiness_host is invalid".to_string());
        }
    }
    Ok(canonical)
}

fn default_opencode_validation_attempts() -> u32 {
    3
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

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_test_config() -> RunnerProfilesConfig {
        RunnerProfilesConfig {
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Opencode,
                command: "sh".to_string(),
                args: vec!["-lc".to_string(), "runner script".to_string()],
                container_image: Some("agent-hub/job-code:locked".to_string()),
                requires_credentials: vec!["openai:oauth".to_string()],
                ..ManagedRunnerSpec::default()
            },
            credential_providers: HashMap::from([(
                "openai:oauth".to_string(),
                serde_json::json!({"type":"opencode-chatgpt-oauth","host_auth_path":"/auth.json","safety_margin_secs":60}),
            )]),
            egress: serde_json::json!({
                "llm":{"default_provider":"openai","providers":{"openai":["auth.openai.com","chatgpt.com"]}}
            }),
            github_hosts: vec!["github.com".to_string()],
            ..RunnerProfilesConfig::default()
        }
    }

    #[test]
    fn canonical_runner_validation_rejects_ambiguous_representations() {
        let valid = canonical_test_config().validated().unwrap();
        assert_eq!(valid.default_agent.requires_credentials, ["openai:oauth"]);

        for mutation in [
            "command_whitespace",
            "empty_argument",
            "argument_whitespace",
            "image_whitespace",
            "zero_attempts",
            "duplicate_credentials",
            "duplicate_hosts",
            "empty_egress_host",
            "credential_name_mismatch",
            "mcp_whitespace",
            "duplicate_provider_casing",
        ] {
            let mut config = canonical_test_config();
            match mutation {
                "command_whitespace" => config.default_agent.command = " sh".to_string(),
                "empty_argument" => config.default_agent.args.push(String::new()),
                "argument_whitespace" => config.default_agent.args[0] = " -lc".to_string(),
                "image_whitespace" => {
                    config.default_agent.container_image = Some("image ".to_string())
                }
                "zero_attempts" => config.default_agent.opencode.max_validation_attempts = 0,
                "duplicate_credentials" => config
                    .default_agent
                    .requires_credentials
                    .push("openai:oauth".to_string()),
                "duplicate_hosts" => config.github_hosts.push("GITHUB.COM".to_string()),
                "empty_egress_host" => {
                    config.egress["llm"]["providers"]["openai"] =
                        serde_json::json!(["chatgpt.com", ""])
                }
                "credential_name_mismatch" => {
                    config.credential_providers.get_mut("openai:oauth").unwrap()["name"] =
                        serde_json::json!("other")
                }
                "mcp_whitespace" => {
                    config.mcp_bridges = vec![serde_json::json!({
                        "name":" bridge",
                        "host_endpoint":{"type":"unix","path":"/tmp/bridge.sock"},
                        "container_socket_path":"/tmp/container.sock"
                    })]
                }
                "duplicate_provider_casing" => {
                    config.egress["llm"]["providers"]["OpenAI"] =
                        serde_json::json!(["auth.openai.com"])
                }
                _ => unreachable!(),
            }
            assert!(config.validated().is_err(), "{mutation}");
        }
    }

    #[test]
    fn canonical_runner_normalizes_host_casing_and_order_once() {
        let mut first = canonical_test_config();
        first.egress = serde_json::json!({
            "llm":{"default_provider":"OPENAI","providers":{"OpenAI":["CHATGPT.COM","auth.openai.com"]}}
        });
        first.github_hosts = vec!["GITHUB.COM".to_string()];
        let first = first.validated().unwrap();
        let second = canonical_test_config().validated().unwrap();
        assert_eq!(
            first.resolve_agent_profile(None).unwrap(),
            second.resolve_agent_profile(None).unwrap()
        );
    }

    #[test]
    fn profile_digest_normalizes_all_set_collections_with_total_keys() {
        let mut first = canonical_test_config()
            .validated()
            .unwrap()
            .resolve_agent_profile(None)
            .unwrap();
        first.file_mounts = vec![
            ManagedRunnerFileMount {
                host_path: "/b".to_string(),
                container_path: "/same".to_string(),
                read_only: false,
            },
            ManagedRunnerFileMount {
                host_path: "/a".to_string(),
                container_path: "/same".to_string(),
                read_only: true,
            },
        ];
        first.credential_providers = vec![
            CredentialProviderDescriptor {
                name: "same".to_string(),
                provider_type: "oauth".to_string(),
                targets: vec!["/b".to_string(), "/a".to_string()],
            },
            CredentialProviderDescriptor {
                name: "same".to_string(),
                provider_type: "api".to_string(),
                targets: vec!["/z".to_string(), "/y".to_string()],
            },
        ];
        first.trusted_mounts = vec![
            TrustedMountDescriptor {
                purpose: "state".to_string(),
                container_path: "/same".to_string(),
                read_only: false,
            },
            TrustedMountDescriptor {
                purpose: "config".to_string(),
                container_path: "/same".to_string(),
                read_only: true,
            },
        ];
        first.mcp_bridges = vec![
            McpBridgeDescriptor {
                name: "same".to_string(),
                endpoint_type: "unix".to_string(),
                target: "b".to_string(),
                container_socket_path: "/b.sock".to_string(),
                args: vec!["second".to_string(), "first".to_string()],
                env_keys: vec!["Z".to_string(), "A".to_string()],
            },
            McpBridgeDescriptor {
                name: "same".to_string(),
                endpoint_type: "unix".to_string(),
                target: "a".to_string(),
                container_socket_path: "/a.sock".to_string(),
                args: Vec::new(),
                env_keys: vec!["Y".to_string(), "B".to_string()],
            },
        ];
        first.github_hosts = vec!["git.example.com".to_string(), "github.com".to_string()];
        first.egress["llm"]["providers"] = serde_json::json!({
            "OpenAI":["CHATGPT.COM","auth.openai.com"],
            "openai":["chatgpt.com"]
        });
        first.egress["proxy"]["declared_volumes"] =
            serde_json::json!(["/var/spool/squid", "/var/log/squid"]);

        let mut reversed = first.clone();
        reversed.file_mounts.reverse();
        reversed.credential_providers.reverse();
        for provider in &mut reversed.credential_providers {
            provider.targets.reverse();
        }
        reversed.trusted_mounts.reverse();
        reversed.mcp_bridges.reverse();
        for bridge in &mut reversed.mcp_bridges {
            bridge.env_keys.reverse();
        }
        reversed.github_hosts.reverse();
        reversed.egress["llm"]["providers"] = serde_json::json!({
            "openai":["AUTH.OPENAI.COM","chatgpt.com"]
        });
        reversed.egress["proxy"]["declared_volumes"] =
            serde_json::json!(["/var/log/squid", "/var/spool/squid"]);

        assert_eq!(
            digest_profile(&first).unwrap(),
            digest_profile(&reversed).unwrap()
        );
        let canonical_first = canonicalize_resolved_runner_profile(&first).unwrap();
        let canonical_reversed = canonicalize_resolved_runner_profile(&reversed).unwrap();
        assert_eq!(canonical_first, canonical_reversed);
        assert_eq!(
            canonical_first.args, first.args,
            "ordered command arguments are preserved"
        );
        reversed
            .mcp_bridges
            .iter_mut()
            .find(|bridge| !bridge.args.is_empty())
            .unwrap()
            .args
            .reverse();
        assert_ne!(
            digest_profile(&first).unwrap(),
            digest_profile(&reversed).unwrap()
        );
    }

    #[test]
    fn omitted_and_explicit_runner_defaults_resolve_to_identical_canonical_profile() {
        let configured = canonical_test_config();
        let mut omitted = serde_json::to_value(&configured).unwrap();
        omitted["default_agent"]
            .as_object_mut()
            .unwrap()
            .remove("opencode");
        let mut explicit = serde_json::to_value(configured).unwrap();
        explicit["default_agent"]["opencode"] = serde_json::json!({
            "max_validation_attempts": default_opencode_validation_attempts()
        });
        let omitted: RunnerProfilesConfig = serde_json::from_value(omitted).unwrap();
        let explicit: RunnerProfilesConfig = serde_json::from_value(explicit).unwrap();
        let omitted = omitted
            .validated()
            .unwrap()
            .resolve_agent_profile(None)
            .unwrap();
        let explicit = explicit
            .validated()
            .unwrap()
            .resolve_agent_profile(None)
            .unwrap();
        assert_eq!(omitted, explicit);
        assert_eq!(
            omitted,
            canonicalize_resolved_runner_profile(&omitted).unwrap()
        );
    }

    #[test]
    fn canonical_profile_digest_binds_command_arguments_image_and_attempts() {
        let base = canonical_test_config().validated().unwrap();
        let base_digest = base.resolve_agent_profile(None).unwrap().config_digest;
        for mutation in ["command", "argument", "image", "attempts"] {
            let mut config = canonical_test_config();
            match mutation {
                "command" => config.default_agent.command = "other".to_string(),
                "argument" => config.default_agent.args[1] = "other script".to_string(),
                "image" => config.default_agent.container_image = Some("other:image".to_string()),
                "attempts" => config.default_agent.opencode.max_validation_attempts = 4,
                _ => unreachable!(),
            }
            let digest = config
                .validated()
                .unwrap()
                .resolve_agent_profile(None)
                .unwrap()
                .config_digest;
            assert_ne!(digest, base_digest, "{mutation}");
        }
    }

    #[test]
    fn canonical_profile_binds_proxy_runtime_contract() {
        let base = canonical_test_config().validated().unwrap();
        let base_profile = base.resolve_agent_profile(None).unwrap();
        assert_eq!(base_profile.egress["proxy"]["runtime_uid"], 13);
        assert_eq!(base_profile.egress["proxy"]["runtime_gid"], 13);
        assert_eq!(base_profile.egress["proxy"]["writable_tmpfs_mode"], "0755");
        assert_eq!(
            base_profile.egress["proxy"]["declared_volumes"],
            serde_json::json!(["/var/log/squid", "/var/spool/squid"])
        );

        for (field, value) in [
            ("runtime_uid", serde_json::json!(14)),
            ("runtime_gid", serde_json::json!(14)),
            ("writable_tmpfs_mode", serde_json::json!("0700")),
        ] {
            let mut config = canonical_test_config();
            config.egress["proxy"][field] = value;
            let profile = config
                .validated()
                .unwrap()
                .resolve_agent_profile(None)
                .unwrap();
            assert_ne!(profile.config_digest, base_profile.config_digest, "{field}");
        }
    }

    #[test]
    fn proxy_runtime_contract_rejects_invalid_ranges_and_modes() {
        for (field, value) in [
            ("runtime_uid", serde_json::json!(-1)),
            ("runtime_gid", serde_json::json!(u64::from(u32::MAX) + 1)),
            ("writable_tmpfs_mode", serde_json::json!("755")),
            ("writable_tmpfs_mode", serde_json::json!("0999")),
            ("declared_volumes", serde_json::json!(["/var/log/squid"])),
            (
                "declared_volumes",
                serde_json::json!(["/var/log/squid", "/var/spool/squid", "/extra"]),
            ),
        ] {
            let mut config = canonical_test_config();
            config.egress["proxy"][field] = value.clone();
            assert!(config.validated().is_err(), "{field}={value}");
        }
    }

    #[test]
    fn profile_digest_binds_every_authorized_runner_security_field() {
        let profile = canonical_test_config()
            .validated()
            .unwrap()
            .resolve_agent_profile(None)
            .unwrap();
        let baseline = profile.config_digest.clone();
        for mutation in [
            "kind",
            "credential_identity",
            "credential_type",
            "credential_targets",
            "egress_policy",
            "provider_hosts",
            "github_hosts",
            "mcp_bridge",
            "trusted_mount",
            "file_mount",
        ] {
            let mut mutated = profile.clone();
            match mutation {
                "kind" => mutated.kind = ManagedRunnerKind::Generic,
                "credential_identity" => {
                    mutated.credential_providers[0].name = "other:credential".to_string()
                }
                "credential_type" => {
                    mutated.credential_providers[0].provider_type = "other-type".to_string()
                }
                "credential_targets" => mutated.credential_providers[0]
                    .targets
                    .push("/other/target".to_string()),
                "egress_policy" => mutated.egress["proxy"]["listen_port"] = serde_json::json!(9999),
                "provider_hosts" => {
                    mutated.egress["llm"]["providers"]["openai"] =
                        serde_json::json!(["different.example.com"])
                }
                "github_hosts" => mutated
                    .github_hosts
                    .push("enterprise.example.com".to_string()),
                "mcp_bridge" => mutated.mcp_bridges.push(McpBridgeDescriptor {
                    name: "bridge".to_string(),
                    endpoint_type: "unix".to_string(),
                    target: "/tmp/source.sock".to_string(),
                    container_socket_path: "/tmp/target.sock".to_string(),
                    args: Vec::new(),
                    env_keys: Vec::new(),
                }),
                "trusted_mount" => mutated.trusted_mounts[0].read_only = false,
                "file_mount" => mutated.file_mounts.push(ManagedRunnerFileMount {
                    host_path: "/host".to_string(),
                    container_path: "/container".to_string(),
                    read_only: true,
                }),
                _ => unreachable!(),
            }
            assert_ne!(digest_profile(&mutated).unwrap(), baseline, "{mutation}");
        }
    }

    #[test]
    fn profile_digest_excludes_credential_values_and_is_deterministic() {
        let runner = ManagedRunnerSpec {
            command: "runner".to_string(),
            requires_credentials: vec!["token".to_string()],
            ..ManagedRunnerSpec::default()
        };
        let mut first = RunnerProfilesConfig {
            managed_runners: HashMap::from([("agent".to_string(), runner.clone())]),
            credential_providers: HashMap::from([(
                "token".to_string(),
                serde_json::json!({"type":"env", "vars":{"TOKEN":"first"}}),
            )]),
            ..RunnerProfilesConfig::default()
        };
        let first_profile = first.resolve_agent_profile(Some("agent")).unwrap();
        first.credential_providers.insert(
            "token".to_string(),
            serde_json::json!({"type":"env", "vars":{"TOKEN":"second"}}),
        );
        let second_profile = first.resolve_agent_profile(Some("agent")).unwrap();
        assert_eq!(first_profile.config_digest, second_profile.config_digest);
        assert_eq!(first_profile.credential_providers[0].targets, ["TOKEN"]);
    }

    #[test]
    fn named_profile_never_falls_back() {
        let config = RunnerProfilesConfig::default();
        assert!(config.resolve_agent_profile(Some("missing")).is_err());
    }

    #[test]
    fn opencode_profile_binds_oauth_identity_and_worker_mount_plan() {
        let config = RunnerProfilesConfig {
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Opencode,
                command: "opencode".to_string(),
                requires_credentials: vec!["openai:chatgpt-oauth".to_string()],
                ..ManagedRunnerSpec::default()
            },
            credential_providers: HashMap::from([(
                "openai:chatgpt-oauth".to_string(),
                serde_json::json!({
                    "type":"opencode-chatgpt-oauth",
                    "host_auth_path":"/not-authorized/by-profile",
                    "safety_margin_secs":300
                }),
            )]),
            ..RunnerProfilesConfig::default()
        };
        let profile = config.resolve_agent_profile(None).unwrap();
        assert_eq!(
            profile.credential_providers[0].provider_type,
            "opencode-chatgpt-oauth"
        );
        assert_eq!(
            profile.credential_providers[0].targets,
            ["/opencode/data/opencode/auth.json"]
        );
        assert_eq!(profile.trusted_mounts.len(), 4);
        assert!(
            profile
                .trusted_mounts
                .iter()
                .any(|mount| { mount.purpose == "environment_bundle" && mount.read_only })
        );
        assert_eq!(profile.opencode.trusted_env, trusted_opencode_runtime_env());
    }

    #[test]
    fn default_opencode_runner_uses_pinned_absolute_executables() {
        let runner = RunnerProfilesConfig::default().default_agent;
        assert_eq!(runner.command, "/usr/bin/sh");
        assert_eq!(runner.args.first().map(String::as_str), Some("-lc"));
        assert!(runner.args[1].contains("/usr/local/bin/opencode run"));
        assert!(!runner.args[1].contains(" then opencode "));
        assert_eq!(
            runner.opencode.trusted_env.get("PATH").map(String::as_str),
            Some(DURABLE_OPENCODE_PATH)
        );
    }

    #[test]
    fn opencode_trusted_environment_is_unique_and_digest_bound() {
        let expected = BTreeMap::from([
            ("HOME".to_string(), "/opencode/state/home".to_string()),
            (
                "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS".to_string(),
                "1".to_string(),
            ),
            (
                "OPENCODE_DISABLE_EXTERNAL_SKILLS".to_string(),
                "1".to_string(),
            ),
            ("OPENCODE_DISABLE_MODELS_FETCH".to_string(), "1".to_string()),
            (
                "OPENCODE_DISABLE_PROJECT_CONFIG".to_string(),
                "1".to_string(),
            ),
            ("OPENCODE_PURE".to_string(), "1".to_string()),
            (
                "OPENCODE_EXPERIMENTAL_WEBSOCKETS".to_string(),
                "0".to_string(),
            ),
            ("PATH".to_string(), DURABLE_OPENCODE_PATH.to_string()),
            ("XDG_CACHE_HOME".to_string(), "/opencode/cache".to_string()),
            (
                "XDG_CONFIG_HOME".to_string(),
                "/opencode/config".to_string(),
            ),
            ("XDG_DATA_HOME".to_string(), "/opencode/data".to_string()),
            ("XDG_STATE_HOME".to_string(), "/opencode/state".to_string()),
        ]);
        assert_eq!(trusted_opencode_runtime_env(), expected);
        assert!(!expected.contains_key("OPENCODE_DISABLE_DEFAULT_PLUGINS"));

        let profile = canonical_test_config()
            .validated()
            .unwrap()
            .resolve_agent_profile(None)
            .unwrap();
        let mut mutated = profile.clone();
        mutated
            .opencode
            .trusted_env
            .insert("OPENCODE_PURE".to_string(), "0".to_string());
        assert_ne!(digest_profile(&mutated).unwrap(), profile.config_digest);
    }

    #[test]
    fn opencode_control_environment_policy_denies_discovery_and_auth_controls() {
        for name in [
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_CONFIG_DIR",
            "OPENCODE_AUTH_CONTENT",
            "OPENCODE_PERMISSION",
            "OPENCODE_MODEL",
            "OPENCODE_PROVIDER",
            "OPENCODE_PLUGIN",
            "OPENCODE_DISABLE_PROJECT_CONFIG",
            "OPENCODE_EXPERIMENTAL_WEBSOCKETS",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "HOME",
            "NODE_PATH",
            "BUN_INSTALL",
            "OPENAI_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "AZURE_OPENAI_API_KEY",
            "AWS_BEARER_TOKEN_BEDROCK",
            "PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "BUN_OPTIONS",
            "PYTHONPATH",
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "RUBYOPT",
            "RUBYLIB",
            "PERL5OPT",
            "PERL5LIB",
            "BASH_ENV",
            "ENV",
            "IFS",
            "SHELLOPTS",
            "GCONV_PATH",
            "LUA_INIT",
            "JAVA_TOOL_OPTIONS",
        ] {
            assert!(
                validate_untrusted_opencode_env_name(name).is_err(),
                "accepted {name}"
            );
        }
        for name in [
            "path",
            "ld_preload",
            "CI",
            "TASK_NAME",
            "PYTHONUNBUFFERED",
            "PYTHONIOENCODING",
            "PERL_UNICODE",
            "RUBY_THREAD_VM_STACK_SIZE",
            "opencode_config",
            "Home",
        ] {
            assert!(
                validate_untrusted_opencode_env_name(name).is_ok(),
                "rejected case-sensitive non-control name {name}"
            );
        }
    }

    #[test]
    fn opencode_project_config_discovery_aliases_are_explicit() {
        assert_eq!(
            OPENCODE_PROJECT_CONFIG_ALIASES,
            [
                "opencode.json",
                "opencode.jsonc",
                ".opencode/opencode.json",
                ".opencode/opencode.jsonc",
            ]
        );
        assert_eq!(
            OPENCODE_PROJECT_CONFIG_ALIASES
                .iter()
                .collect::<BTreeSet<_>>()
                .len(),
            OPENCODE_PROJECT_CONFIG_ALIASES.len()
        );
    }

    #[test]
    fn opencode_profile_rejects_host_mounts() {
        let config = RunnerProfilesConfig {
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Opencode,
                command: "opencode".to_string(),
                file_mounts: vec![ManagedRunnerFileMount {
                    host_path: "/var/run/docker.sock".to_string(),
                    container_path: "/var/run/docker.sock".to_string(),
                    read_only: false,
                }],
                ..ManagedRunnerSpec::default()
            },
            ..RunnerProfilesConfig::default()
        };
        assert!(config.resolve_agent_profile(None).is_err());
    }

    #[test]
    fn profile_rejects_duplicate_mcp_bridge_names() {
        let bridge = serde_json::json!({
            "name":"duplicate",
            "host_endpoint":{"type":"unix","path":"/tmp/source.sock"},
            "container_socket_path":"/tmp/target.sock"
        });
        let config = RunnerProfilesConfig {
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Generic,
                command: "true".to_string(),
                ..ManagedRunnerSpec::default()
            },
            mcp_bridges: vec![bridge.clone(), bridge],
            ..RunnerProfilesConfig::default()
        };
        assert!(config.resolve_agent_profile(None).is_err());
    }
}

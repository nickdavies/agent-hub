use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    #[serde(
        default = "default_opencode_validation_attempts",
        deserialize_with = "deserialize_u32_lenient"
    )]
    pub max_validation_attempts: u32,
}

impl Default for OpencodeRunnerConfig {
    fn default() -> Self {
        Self {
            max_validation_attempts: default_opencode_validation_attempts(),
        }
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
    pub mcp_bridges: Vec<McpBridgeDescriptor>,
    pub egress: serde_json::Value,
    pub github_hosts: Vec<String>,
    pub opencode: OpencodeRunnerConfig,
    pub config_digest: String,
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
    pub fn resolve_agent_profile(
        &self,
        requested_name: Option<&str>,
    ) -> Result<ResolvedRunnerProfile, String> {
        let requested = requested_name
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "auto")
            .or_else(|| {
                self.default_agent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            });
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
        let name = operation.trim();
        if name.is_empty() {
            return Err("managed operation must not be empty".to_string());
        }
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
        normalize_runner_spec(&mut spec);
        if spec.command.is_empty() {
            return Err(format!("managed runner profile '{name}' has no command"));
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
        credential_providers.sort_by(|left, right| left.name.cmp(&right.name));
        credential_providers.dedup_by(|left, right| left.name == right.name);
        let mut mcp_bridges = self
            .mcp_bridges
            .iter()
            .map(mcp_bridge_descriptor)
            .collect::<Result<Vec<_>, _>>()?;
        mcp_bridges.sort_by(|left, right| left.name.cmp(&right.name));
        mcp_bridges.dedup_by(|left, right| left.name == right.name);
        let mut github_hosts = self
            .github_hosts
            .iter()
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect::<Vec<_>>();
        github_hosts.sort();
        github_hosts.dedup();

        let mut profile = ResolvedRunnerProfile {
            profile_name: name,
            kind: spec.kind,
            command: spec.command,
            args: spec.args,
            container_image: spec.container_image,
            file_mounts: spec.file_mounts,
            credential_providers,
            mcp_bridges,
            egress: canonical_set_json(merge_json_defaults(
                default_egress_value(),
                self.egress.clone(),
            )),
            github_hosts,
            opencode: spec.opencode,
            config_digest: String::new(),
        };
        profile.config_digest = digest_profile(&profile)?;
        Ok(profile)
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
                "raw.githubusercontent.com",
                "objects.githubusercontent.com"
            ],
            "go_registry_hosts": [
                "proxy.golang.org",
                "sum.golang.org",
                "storage.googleapis.com"
            ],
            "crates_registry_hosts": [
                "crates.io",
                "index.crates.io",
                "static.crates.io"
            ]
        },
        "proxy": {
            "image": "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029",
            "listen_port": 3128
        },
        "preflight_image": null
    })
}

pub fn digest_profile(profile: &ResolvedRunnerProfile) -> Result<String, String> {
    let mut canonical = profile.clone();
    canonical.config_digest.clear();
    canonical.file_mounts.sort_by(|left, right| {
        (&left.host_path, &left.container_path, left.read_only).cmp(&(
            &right.host_path,
            &right.container_path,
            right.read_only,
        ))
    });
    canonical.credential_providers.sort_by(|left, right| {
        (&left.name, &left.provider_type, &left.targets).cmp(&(
            &right.name,
            &right.provider_type,
            &right.targets,
        ))
    });
    canonical
        .mcp_bridges
        .sort_by(|left, right| left.name.cmp(&right.name));
    let bytes = serde_json::to_vec(&canonical).map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(bytes)))
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

fn string_target(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Vec<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default()
}

fn normalize_runner_spec(spec: &mut ManagedRunnerSpec) {
    spec.command = spec.command.trim().to_string();
    spec.container_image = spec
        .container_image
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    spec.file_mounts.retain(|mount| {
        !mount.host_path.trim().is_empty() && !mount.container_path.trim().is_empty()
    });
    for mount in &mut spec.file_mounts {
        mount.host_path = mount.host_path.trim().to_string();
        mount.container_path = mount.container_path.trim().to_string();
    }
    spec.requires_credentials = spec
        .requires_credentials
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    spec.requires_credentials.sort();
    spec.requires_credentials.dedup();
}

fn default_opencode_validation_attempts() -> u32 {
    3
}

fn deserialize_u32_lenient<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_u64()
                && let Ok(value) = u32::try_from(value)
            {
                return Ok(value);
            }
            if let Some(value) = number.as_f64()
                && value >= 0.0
                && value <= f64::from(u32::MAX)
                && value.fract() == 0.0
            {
                return Ok(value as u32);
            }
            Err(D::Error::custom("expected an unsigned 32-bit integer"))
        }
        other => Err(D::Error::custom(format!("expected a number, got {other}"))),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

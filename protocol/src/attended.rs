use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    CanonicalRuntimeSpec, ExecutionMode, NetworkResourceKind, NetworkResourceRequest,
    ResolvedAuthorizationBehavior,
};

pub const ATTENDED_AUTHORIZATION_REQUEST_VERSION: &str = "attended_authorization_request_v1";
pub const ATTENDED_AUTHORIZATION_VERSION: u32 = 1;
pub const APPROVED_EXECUTION_PIDS_LIMIT: u32 = 512;
const ATTENDED_AUTHORIZATION_DOMAIN_V1: &[u8] = b"agent-hub:attended-authorization:v1\0";

pub fn canonical_context_root_path(alias: &str) -> Option<&'static str> {
    match alias {
        "code" => Some("/home/nick.davies/code"),
        "oss" => Some("/home/nick.davies/oss"),
        _ => None,
    }
}

pub fn is_canonical_context_root_path(path: &str) -> bool {
    ["/home/nick.davies/code", "/home/nick.davies/oss"]
        .iter()
        .any(|root| {
            path == *root
                || path
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedAuthorizationRequestV1 {
    pub version: String,
    pub proposal: AttendedProposal,
    pub execution: AttendedExecutionSpec,
    pub output: AttendedOutputContract,
    pub lifecycle: AttendedLifecycleContract,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedProposal {
    pub environment: String,
    pub image_family: String,
    pub image_tag: String,
    pub image_id: String,
    pub network_class: String,
    pub network_resources: Vec<NetworkResourceKind>,
    pub tool_approval: ToolApprovalMode,
    pub overlays: Vec<String>,
    pub timeout_secs: u64,
    pub prompt_digest: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalMode {
    Auto,
    Dippy,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovedExecutionSpec {
    pub runtime: CanonicalRuntimeSpec,
    pub behavior: ResolvedAuthorizationBehavior,
    pub pids_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedExecutionSpec {
    pub config_root: String,
    pub config_digest: String,
    pub config_provenance: BTreeMap<String, String>,
    pub environment_bundle_digest: String,
    #[serde(flatten)]
    pub approved: ApprovedExecutionSpec,
    pub credential_providers: Vec<AuthorizedCredentialProvider>,
    pub overlays: Vec<AuthorizedOverlay>,
    pub workspaces: Vec<WorkspaceSpec>,
    pub context_roots: Vec<ContextRootSpec>,
    pub repositories: Vec<RepositoryMountSpec>,
    pub primary_workspace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedOverlay {
    pub name: String,
    pub provider: String,
    pub config: String,
}

impl std::ops::Deref for AttendedExecutionSpec {
    type Target = ApprovedExecutionSpec;

    fn deref(&self) -> &Self::Target {
        &self.approved
    }
}

impl std::ops::DerefMut for AttendedExecutionSpec {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.approved
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedCredentialProvider {
    pub name: String,
    pub provider_type: String,
    pub source: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub validity: Option<String>,
    pub targets: Vec<String>,
    pub access: String,
    pub constraints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    pub alias: String,
    pub host_path: String,
    pub container_path: String,
    pub access: WorkspaceAccess,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ContextRootSpec {
    pub alias: String,
    pub host_path: String,
    pub container_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMountSpec {
    pub workspace_alias: String,
    pub repository_root: String,
    pub mount_path: String,
    pub remote: Option<String>,
    pub access: WorkspaceAccess,
    pub starting: RepositoryState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct RepositoryState {
    pub head: Option<String>,
    pub reference: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub dirty_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedOutputContract {
    pub transcript: String,
    pub result: String,
    pub artifacts: String,
    pub repositories: String,
    pub cleanup: String,
    pub limits: AttendedOutputLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedOutputLimits {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub transcript_bytes: u64,
    pub artifact_bytes: u64,
}

impl Default for AttendedOutputContract {
    fn default() -> Self {
        Self {
            transcript: "transcript.jsonl".into(),
            result: "result.json".into(),
            artifacts: "artifacts.json".into(),
            repositories: "repositories.json".into(),
            cleanup: "cleanup.json".into(),
            limits: AttendedOutputLimits {
                stdout_bytes: 16 * 1024 * 1024,
                stderr_bytes: 16 * 1024 * 1024,
                transcript_bytes: 32 * 1024 * 1024,
                artifact_bytes: 4 * 1024 * 1024,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedLifecycleContract {
    pub preserve_workspace_on_failure: bool,
    pub cancellation_requires_owner: bool,
    pub cleanup_is_explicit: bool,
}

impl Default for AttendedLifecycleContract {
    fn default() -> Self {
        Self {
            preserve_workspace_on_failure: true,
            cancellation_requires_owner: true,
            cleanup_is_explicit: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedAuthorizationArtifact {
    pub version: u32,
    pub digest: String,
    #[serde(with = "hex_bytes")]
    #[schemars(with = "String")]
    pub canonical_bytes: Vec<u8>,
}

impl AttendedAuthorizationRequestV1 {
    pub fn normalize(mut self) -> Result<Self, String> {
        if self.version != ATTENDED_AUTHORIZATION_REQUEST_VERSION {
            return Err(format!(
                "unsupported attended authorization schema {}",
                self.version
            ));
        }
        if self.proposal.prompt.is_empty() {
            return Err("attended prompt must not be empty".into());
        }
        self.execution.approved = self.execution.approved.clone().normalize();
        if self.execution.approved.pids_limit == 0 {
            return Err("approved execution PID limit must be positive".into());
        }
        let prompt_digest = sha256_hex_with_domain(
            b"agent-hub:attended-prompt:v1\0",
            self.proposal.prompt.as_bytes(),
        );
        if self.proposal.prompt_digest != prompt_digest {
            return Err("attended prompt digest does not match prompt bytes".into());
        }
        if self.proposal.environment.is_empty()
            || self.proposal.image_family.is_empty()
            || self.proposal.network_class.is_empty()
            || self.execution.config_root.is_empty()
        {
            return Err("attended selectors and config root must not be empty".into());
        }
        if self.execution.config_provenance.is_empty() {
            return Err("attended config provenance must not be empty".into());
        }
        if self.proposal.image_family != "job-code" {
            return Err("attended image family must be 'job-code'".into());
        }
        if !["none", "llm-only", "build", "guarded-web"]
            .contains(&self.proposal.network_class.as_str())
        {
            return Err("unsupported attended network class selector".into());
        }
        let timeout = self
            .execution
            .runtime
            .environment
            .timeout_secs
            .ok_or_else(|| "attended execution requires a deadline".to_string())?;
        if timeout == 0 || timeout > 86_400 {
            return Err("attended execution timeout must be between 1 and 86400 seconds".into());
        }
        if self.execution.runtime.environment.execution_mode != ExecutionMode::Docker {
            return Err("attended execution must use Docker".into());
        }
        let image = self
            .execution
            .runtime
            .environment
            .container_image
            .as_deref()
            .ok_or_else(|| "attended execution requires an exact image".to_string())?;
        if image_family(image) != self.proposal.image_family {
            return Err("attended image selector does not match the exact image".into());
        }
        if image != self.proposal.image_tag {
            return Err("attended exact image tag does not match the canonical runtime".into());
        }
        validate_image_id(&self.proposal.image_id)?;
        if self.proposal.timeout_secs != timeout {
            return Err("attended timeout selector does not match the canonical runtime".into());
        }
        self.proposal.overlays.sort();
        if self
            .proposal
            .overlays
            .windows(2)
            .any(|items| items[0] == items[1])
        {
            return Err("duplicate attended overlay selector".into());
        }
        self.proposal
            .network_resources
            .sort_by_key(ToString::to_string);
        if self
            .proposal
            .network_resources
            .windows(2)
            .any(|resources| resources[0] == resources[1])
        {
            return Err("duplicate attended network resource selector".into());
        }
        if self
            .proposal
            .network_resources
            .contains(&NetworkResourceKind::GuardedWeb)
            && self.proposal.network_resources.len() != 1
        {
            return Err("guarded_web cannot be combined with other network resources".into());
        }
        if !self.proposal.network_resources.iter().all(|resource| {
            match self.proposal.network_class.as_str() {
                "none" => false,
                "llm-only" => *resource == NetworkResourceKind::Llm,
                "build" => *resource != NetworkResourceKind::GuardedWeb,
                "guarded-web" => *resource == NetworkResourceKind::GuardedWeb,
                _ => false,
            }
        }) {
            return Err("attended network resources exceed the selected network class".into());
        }
        if self.output != AttendedOutputContract::default()
            || self.lifecycle != AttendedLifecycleContract::default()
        {
            return Err("attended V1 output and lifecycle contracts are fixed".into());
        }
        if self
            .execution
            .behavior
            .prompt
            .as_ref()
            .map(|prompt| prompt.user.as_str())
            != Some(self.proposal.prompt.as_str())
        {
            return Err("attended proposal and execution prompts differ".into());
        }
        let expected_network = if self.proposal.network_resources.is_empty() {
            NetworkResourceRequest::None
        } else {
            NetworkResourceRequest::Resources(self.proposal.network_resources.clone())
        };
        if self
            .execution
            .runtime
            .environment
            .network_resources
            .as_ref()
            != Some(&expected_network)
        {
            return Err("attended network selector does not match the canonical runtime".into());
        }
        let profile_command = self
            .execution
            .behavior
            .runner_profile
            .get("command")
            .and_then(serde_json::Value::as_str);
        let profile_args = self
            .execution
            .behavior
            .runner_profile
            .get("args")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok());
        if profile_command != Some(self.execution.behavior.command.as_str())
            || profile_args.as_ref() != Some(&self.execution.behavior.args)
        {
            return Err("attended runner command differs from its canonical profile".into());
        }
        validate_absolute_path(&self.execution.config_root, "config root")?;
        self.execution.workspaces.sort();
        self.execution.context_roots.sort();
        self.execution.repositories.sort();
        self.execution.credential_providers.sort();
        self.execution.overlays.sort();
        reject_duplicate_by(
            &self.execution.workspaces,
            |item| &item.alias,
            "workspace alias",
        )?;
        reject_duplicate_by(&self.execution.overlays, |item| &item.name, "overlay")?;
        if self
            .execution
            .overlays
            .iter()
            .map(|item| &item.name)
            .collect::<Vec<_>>()
            != self.proposal.overlays.iter().collect::<Vec<_>>()
        {
            return Err("attended overlay descriptors do not match proposal selectors".into());
        }
        reject_duplicate_by(
            &self.execution.credential_providers,
            |item| &item.name,
            "credential provider",
        )?;
        reject_duplicate_by(
            &self.execution.context_roots,
            |item| &item.alias,
            "context alias",
        )?;
        reject_duplicate_by(
            &self.execution.repositories,
            |item| &item.workspace_alias,
            "repository workspace alias",
        )?;
        for context in &self.execution.context_roots {
            if self
                .execution
                .workspaces
                .iter()
                .any(|workspace| workspace.alias == context.alias)
            {
                return Err(format!("duplicate mount alias '{}'", context.alias));
            }
            validate_alias(&context.alias)?;
            validate_absolute_path(&context.host_path, "context root")?;
            if !is_canonical_context_root_path(&context.host_path) {
                return Err("attended context roots are limited to /home/nick.davies/code and /home/nick.davies/oss".into());
            }
            if context.container_path != format!("/context/{}", context.alias) {
                return Err("attended context mount destination is not canonical".into());
            }
        }
        for workspace in &self.execution.workspaces {
            validate_alias(&workspace.alias)?;
            validate_absolute_path(&workspace.host_path, "workspace")?;
            if workspace.container_path != format!("/workspace/{}", workspace.alias) {
                return Err("attended workspace mount destination is not canonical".into());
            }
        }
        if self.execution.workspaces.is_empty()
            || !self
                .execution
                .workspaces
                .iter()
                .any(|item| item.alias == self.execution.primary_workspace)
        {
            return Err("primary workspace is not an approved workspace".into());
        }
        let workspace_aliases = self
            .execution
            .workspaces
            .iter()
            .map(|workspace| workspace.alias.as_str())
            .collect::<BTreeSet<_>>();
        let repository_aliases = self
            .execution
            .repositories
            .iter()
            .map(|repository| repository.workspace_alias.as_str())
            .collect::<BTreeSet<_>>();
        if workspace_aliases != repository_aliases {
            return Err("attended repository identities do not match the workspace set".into());
        }
        for repository in &self.execution.repositories {
            let workspace = self
                .execution
                .workspaces
                .iter()
                .find(|workspace| workspace.alias == repository.workspace_alias)
                .expect("workspace and repository alias sets match");
            if repository.repository_root != workspace.host_path
                || repository.mount_path != workspace.container_path
                || repository.access != workspace.access
            {
                return Err("attended repository mount differs from its workspace".into());
            }
            if repository.starting.detached != repository.starting.branch.is_none()
                && repository.starting.head.is_some()
            {
                return Err("attended repository branch and detached state disagree".into());
            }
        }
        Ok(self)
    }

    pub fn canonical_artifact(&self) -> Result<AttendedAuthorizationArtifact, String> {
        let normalized = self.clone().normalize()?;
        let bytes =
            canonical_json_bytes(serde_json::to_value(normalized).map_err(|e| e.to_string())?)?;
        Ok(AttendedAuthorizationArtifact {
            version: ATTENDED_AUTHORIZATION_VERSION,
            digest: sha256_hex_with_domain(ATTENDED_AUTHORIZATION_DOMAIN_V1, &bytes),
            canonical_bytes: bytes,
        })
    }
}

fn validate_image_id(identity: &str) -> Result<(), String> {
    if identity.len() != 71
        || !identity.starts_with("sha256:")
        || !identity[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("attended image ID is not an immutable sha256 identity".into());
    }
    Ok(())
}

impl AttendedAuthorizationArtifact {
    pub fn verify(&self) -> Result<AttendedAuthorizationRequestV1, String> {
        if self.version != ATTENDED_AUTHORIZATION_VERSION {
            return Err(format!(
                "unsupported attended authorization version {}",
                self.version
            ));
        }
        let expected =
            sha256_hex_with_domain(ATTENDED_AUTHORIZATION_DOMAIN_V1, &self.canonical_bytes);
        if self.digest != expected {
            return Err("attended authorization digest mismatch".into());
        }
        let parsed: AttendedAuthorizationRequestV1 = serde_json::from_slice(&self.canonical_bytes)
            .map_err(|e| format!("invalid attended authorization: {e}"))?;
        let normalized = parsed.normalize()?;
        let canonical =
            canonical_json_bytes(serde_json::to_value(&normalized).map_err(|e| e.to_string())?)?;
        if canonical != self.canonical_bytes {
            return Err("attended authorization payload is not canonical".into());
        }
        Ok(normalized)
    }
}

fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty()
        || alias.len() > 63
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !alias.as_bytes()[0].is_ascii_alphanumeric()
        || !alias.as_bytes()[alias.len() - 1].is_ascii_alphanumeric()
    {
        return Err(format!("mount alias '{alias}' is not canonical"));
    }
    if ["artifacts", "config", "context", "opencode", "output"].contains(&alias) {
        return Err(format!("mount alias '{alias}' is reserved"));
    }
    Ok(())
}

fn validate_absolute_path(value: &str, label: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || !path.is_absolute()
        || value.contains(',')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("attended {label} path is not canonical"));
    }
    Ok(())
}

fn image_family(image: &str) -> String {
    let without_digest = image.split('@').next().unwrap_or(image);
    let slash = without_digest.rfind('/');
    let family = match without_digest.rfind(':') {
        Some(colon) if slash.is_none_or(|slash| colon > slash) => &without_digest[..colon],
        _ => without_digest,
    };
    family.rsplit('/').next().unwrap_or(family).to_string()
}

fn reject_duplicate_by<'a, T, F>(items: &'a [T], key: F, label: &str) -> Result<(), String>
where
    F: Fn(&'a T) -> &'a str,
{
    for pair in items.windows(2) {
        if key(&pair[0]) == key(&pair[1]) {
            return Err(format!("duplicate {label} '{}'", key(&pair[0])));
        }
    }
    Ok(())
}

fn canonical_json_bytes(value: serde_json::Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&canonical_json(value)).map_err(|e| e.to_string())
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted = object
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_value(sorted).expect("JSON object remains serializable")
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        value => value,
    }
}

pub fn sha256_hex_with_domain(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    hex::encode(digest.finalize())
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        hex::decode(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

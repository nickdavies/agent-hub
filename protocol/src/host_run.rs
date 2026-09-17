//! Canonical protocol for a host-owned, one-step attended run.

use std::path::Path;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AttendedAuthorizationArtifact, AttendedOutputContract, AttendedRetryConstraints,
    NetworkResourceKind, WorkspaceAccess, sha256_hex_with_domain,
};

pub const AGENT_HUB_RUN_PERMISSION_NAME: &str = "agent_hub_run_resolved";
pub const AGENT_HUB_RUN_INTENT_SCHEMA_V1: &str = "agent_hub_run_intent_v1";
pub const AGENT_HUB_RUN_RESOLVED_SCHEMA_V1: &str = "agent_hub_run_resolved_v1";
pub const AGENT_HUB_RUN_DECISION_SCHEMA_V1: &str = "agent_hub_run_decision_v1";
pub const AGENT_HUB_RUN_APPROVAL_RECEIPT_SCHEMA_V1: &str = "agent_hub_run_approval_receipt_v1";
pub const AGENT_HUB_RUN_OUTPUT_SCHEMA_V1: &str = "agent_hub_run_output_v1";
pub const AGENT_HUB_RUN_MAX_PROMPT_BYTES: usize = 128 * 1024;
pub const AGENT_HUB_RUN_MAX_MOUNTS: usize = 16;
pub const AGENT_HUB_RUN_MAX_SELECTORS: usize = 16;
pub const AGENT_HUB_RUN_MAX_PATH_BYTES: usize = 1024;
pub const AGENT_HUB_RUN_MAX_CANONICAL_BYTES: usize = 512 * 1024;
pub const AGENT_HUB_RUN_MAX_TIMEOUT_SECS: u64 = 86_400;
pub const AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES: usize = 1024 * 1024;
pub const AGENT_HUB_RUN_MAX_GATEWAY_OUTPUT_BYTES: usize = 64 * 1024;
pub const AGENT_HUB_RUN_MAX_POLICY_PROJECTION_BYTES: usize = 128 * 1024;
pub const AGENT_HUB_RUN_MAX_REASON_BYTES: usize = 4096;
pub const AGENT_HUB_RUN_MAX_OVERLAY_NAME_BYTES: usize = 128;
pub const AGENT_HUB_RUN_MAX_APPROVAL_TIMEOUT_SECS: u64 = 600;
pub const AGENT_HUB_RUN_POLICY_PROJECTION_SCHEMA_V1: &str = "agent_hub_run_policy_projection_v1";
const RESOLVED_DOMAIN: &[u8] = b"agent-hub:agent-hub-run-resolved:v1\0";
const RECEIPT_DOMAIN: &[u8] = b"agent-hub:agent-hub-run-receipt:v1\0";

/// Public tool input. It contains requested capabilities only; config identity,
/// image identity, provider commands, and gateway transport are host-owned.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunIntentV1 {
    pub schema: AgentHubRunIntentSchema,
    #[schemars(length(min = 1, max = 128))]
    pub environment: String,
    pub image_family: AgentHubRunImageFamily,
    pub network_class: AgentHubRunNetworkClass,
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub network_resources: Vec<NetworkResourceKind>,
    pub tool_approval: AgentHubRunToolApproval,
    #[serde(default)]
    #[schemars(length(max = 16), inner(length(min = 1, max = 128)))]
    pub overlays: Vec<String>,
    #[schemars(range(min = 1, max = 86400))]
    pub timeout_secs: u64,
    #[schemars(length(min = 1, max = 131072))]
    pub prompt: String,
    #[schemars(length(min = 1, max = 16))]
    pub workspaces: Vec<AgentHubRunWorkspaceV1>,
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub context_roots: Vec<AgentHubRunContextRootV1>,
    #[schemars(length(min = 1, max = 63))]
    pub primary_workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1024))]
    pub workspace_export_root: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AgentHubRunIntentSchema {
    #[serde(rename = "agent_hub_run_intent_v1")]
    V1,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum AgentHubRunImageFamily {
    #[serde(rename = "job-code")]
    JobCode,
}

impl AgentHubRunImageFamily {
    pub fn as_str(self) -> &'static str {
        "job-code"
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "kebab-case")]
pub enum AgentHubRunNetworkClass {
    None,
    LlmOnly,
    Build,
    GuardedWeb,
}

impl AgentHubRunNetworkClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LlmOnly => "llm-only",
            Self::Build => "build",
            Self::GuardedWeb => "guarded-web",
        }
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubRunToolApproval {
    Auto,
    Dippy,
}

impl AgentHubRunToolApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dippy => "dippy",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunWorkspaceV1 {
    #[schemars(length(min = 1, max = 63))]
    pub alias: String,
    #[schemars(length(min = 1, max = 1024))]
    pub path: String,
    pub access: WorkspaceAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunContextRootV1 {
    #[schemars(length(min = 1, max = 63))]
    pub alias: String,
    #[schemars(length(min = 1, max = 1024))]
    pub path: String,
}

impl AgentHubRunIntentV1 {
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.environment.is_empty()
            || self.environment.len() > 128
            || self.prompt.is_empty()
            || self.prompt.len() > AGENT_HUB_RUN_MAX_PROMPT_BYTES
            || self.primary_workspace.is_empty()
            || self.workspaces.is_empty()
            || self.workspaces.len() > AGENT_HUB_RUN_MAX_MOUNTS
            || self.context_roots.len() > AGENT_HUB_RUN_MAX_MOUNTS
            || self.network_resources.len() > AGENT_HUB_RUN_MAX_SELECTORS
            || self.overlays.len() > AGENT_HUB_RUN_MAX_SELECTORS
            || self.overlays.iter().any(|overlay| {
                overlay.is_empty() || overlay.len() > AGENT_HUB_RUN_MAX_OVERLAY_NAME_BYTES
            })
            || !(1..=AGENT_HUB_RUN_MAX_TIMEOUT_SECS).contains(&self.timeout_secs)
        {
            return Err("agent_hub_run intent exceeds a required bound".into());
        }
        let mut aliases = std::collections::BTreeSet::new();
        let mut paths = std::collections::BTreeSet::new();
        for item in &self.workspaces {
            validate_alias(&item.alias)?;
            validate_absolute_path(&item.path)?;
            if !aliases.insert(&item.alias) || !paths.insert(&item.path) {
                return Err("agent_hub_run aliases and paths must be unique".into());
            }
        }
        for item in &self.context_roots {
            validate_alias(&item.alias)?;
            validate_absolute_path(&item.path)?;
            if !aliases.insert(&item.alias) || !paths.insert(&item.path) {
                return Err("agent_hub_run aliases and paths must be unique".into());
            }
        }
        if let Some(export_root) = &self.workspace_export_root {
            validate_absolute_path(export_root)?;
            let matches = self.workspaces.iter().filter(|workspace| {
                workspace.access == WorkspaceAccess::ReadWrite
                    && Path::new(export_root) == Path::new(&workspace.path).join(".agent-hub/runs")
            });
            if matches.count() != 1 {
                return Err(
                    "workspace export root must be exactly .agent-hub/runs under one read-write workspace"
                        .into(),
                );
            }
        }
        if !self
            .workspaces
            .iter()
            .any(|item| item.alias == self.primary_workspace)
        {
            return Err("primary workspace is not requested".into());
        }
        if self
            .network_resources
            .iter()
            .enumerate()
            .any(|(index, item)| self.network_resources[..index].contains(item))
            || self
                .overlays
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.overlays.len()
        {
            return Err("agent_hub_run selectors must be unique".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunRootProjectionV1 {
    #[schemars(length(min = 1, max = 63))]
    pub alias: String,
    #[schemars(length(min = 1, max = 1024))]
    pub path: String,
    pub access: WorkspaceAccess,
    #[schemars(length(min = 1, max = 128))]
    pub root_identity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunCapabilityProjectionV1 {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    #[schemars(length(min = 1, max = 64))]
    pub access: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunReviewProjectionV1 {
    #[schemars(length(min = 1, max = 128))]
    pub config_digest: String,
    #[schemars(length(min = 1, max = 128))]
    pub environment_bundle_digest: String,
    #[schemars(length(min = 1, max = 128))]
    pub approval_bundle_digest: String,
    #[schemars(length(min = 1, max = 63))]
    pub primary_workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1024))]
    pub workspace_export_root: Option<String>,
}

/// Strict policy surface derived from a verified canonical execution. Policy
/// matching never receives the canonical transport or public intent fields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunPolicyProjectionV1 {
    pub schema: String,
    #[schemars(length(min = 1, max = 128))]
    pub environment: String,
    #[schemars(length(min = 1, max = 64))]
    pub image_family: String,
    #[schemars(length(min = 71, max = 71))]
    pub image_id: String,
    #[schemars(length(min = 1, max = 32))]
    pub network_class: String,
    #[schemars(length(max = 16))]
    pub network_resources: Vec<NetworkResourceKind>,
    #[schemars(length(min = 1, max = 16))]
    pub tool_approval: String,
    #[schemars(length(min = 1, max = 16))]
    pub workspaces: Vec<AgentHubRunRootProjectionV1>,
    #[schemars(length(max = 16))]
    pub context_roots: Vec<AgentHubRunRootProjectionV1>,
    #[schemars(length(max = 16))]
    pub overlays: Vec<AgentHubRunCapabilityProjectionV1>,
    #[schemars(length(max = 64))]
    pub capabilities: Vec<AgentHubRunCapabilityProjectionV1>,
    #[schemars(range(min = 1, max = 86400))]
    pub timeout_secs: u64,
    pub retry: AttendedRetryConstraints,
    pub output: AttendedOutputContract,
    pub prompt_digest: String,
    pub review: AgentHubRunReviewProjectionV1,
}

impl AgentHubRunPolicyProjectionV1 {
    pub fn derive(artifact: &AttendedAuthorizationArtifact) -> Result<Self, String> {
        let request = artifact.verify_v3()?;
        let workspace_identity = |alias: &str| {
            request
                .workspace_root_identities
                .get(alias)
                .cloned()
                .ok_or_else(|| format!("workspace {alias} has no root identity"))
        };
        let context_identity = |alias: &str| {
            request
                .context_root_identities
                .get(alias)
                .cloned()
                .ok_or_else(|| format!("context root {alias} has no root identity"))
        };
        let mut workspaces = request
            .execution
            .workspaces
            .iter()
            .map(|root| {
                Ok(AgentHubRunRootProjectionV1 {
                    alias: root.alias.clone(),
                    path: root.host_path.clone(),
                    access: root.access,
                    root_identity: workspace_identity(&root.alias)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut context_roots = request
            .execution
            .context_roots
            .iter()
            .map(|root| {
                Ok(AgentHubRunRootProjectionV1 {
                    alias: root.alias.clone(),
                    path: root.host_path.clone(),
                    access: WorkspaceAccess::ReadOnly,
                    root_identity: context_identity(&root.alias)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        workspaces.sort_by(|left, right| left.alias.cmp(&right.alias));
        context_roots.sort_by(|left, right| left.alias.cmp(&right.alias));
        let mut overlays = request
            .execution
            .overlays
            .iter()
            .map(|overlay| {
                let config: serde_json::Value = serde_json::from_str(&overlay.config)
                    .map_err(|error| format!("invalid overlay projection config: {error}"))?;
                let access = config
                    .get("access")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("overlay {} has no reviewed access", overlay.name))?;
                Ok(AgentHubRunCapabilityProjectionV1 {
                    name: overlay.name.clone(),
                    access: access.to_string(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        overlays.sort();
        let mut capabilities = request
            .execution
            .credential_providers
            .iter()
            .map(|capability| AgentHubRunCapabilityProjectionV1 {
                name: capability.name.clone(),
                access: capability.access.clone(),
            })
            .collect::<Vec<_>>();
        capabilities.extend(request.execution.runtime.github_capabilities.iter().map(
            |capability| {
                let access = if capability.operations.iter().any(|operation| {
                    !matches!(
                        operation,
                        crate::GithubOperation::ReadRepository
                            | crate::GithubOperation::ReadPullRequest
                    )
                }) {
                    "read_write"
                } else {
                    "read_only"
                };
                AgentHubRunCapabilityProjectionV1 {
                    name: format!("github:{}", capability.repository),
                    access: access.into(),
                }
            },
        ));
        capabilities.sort();
        capabilities.dedup();
        let projection = Self {
            schema: AGENT_HUB_RUN_POLICY_PROJECTION_SCHEMA_V1.into(),
            environment: request.proposal.environment.clone(),
            image_family: request.proposal.image_family.clone(),
            image_id: request.proposal.image_id.clone(),
            network_class: request.proposal.network_class.clone(),
            network_resources: request.proposal.network_resources.clone(),
            tool_approval: match request.proposal.tool_approval {
                crate::ToolApprovalMode::Auto => "auto",
                crate::ToolApprovalMode::Dippy => "dippy",
            }
            .into(),
            workspaces,
            context_roots,
            overlays,
            capabilities,
            timeout_secs: request.proposal.timeout_secs,
            retry: request.retry.clone(),
            output: request.output.clone(),
            prompt_digest: request.proposal.prompt_digest.clone(),
            review: AgentHubRunReviewProjectionV1 {
                config_digest: request.execution.config_digest.clone(),
                environment_bundle_digest: request.execution.environment_bundle_digest.clone(),
                approval_bundle_digest: request.approval_bundle_digest.clone(),
                primary_workspace: request.execution.primary_workspace.clone(),
                workspace_export_root: request
                    .workspace_export
                    .as_ref()
                    .map(|item| item.root.clone()),
            },
        };
        projection.validate()?;
        Ok(projection)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != AGENT_HUB_RUN_POLICY_PROJECTION_SCHEMA_V1
            || self.environment.is_empty()
            || self.environment.len() > 128
            || self.image_family.is_empty()
            || self.image_family.len() > 64
            || !["none", "llm-only", "build", "guarded-web"].contains(&self.network_class.as_str())
            || !["auto", "dippy"].contains(&self.tool_approval.as_str())
            || self.workspaces.is_empty()
            || self.workspaces.len() > AGENT_HUB_RUN_MAX_MOUNTS
            || self.context_roots.len() > AGENT_HUB_RUN_MAX_MOUNTS
            || self.network_resources.len() > AGENT_HUB_RUN_MAX_SELECTORS
            || self.overlays.len() > AGENT_HUB_RUN_MAX_SELECTORS
            || self.capabilities.len() > 64
            || self.prompt_digest.len() != 64
            || !(1..=AGENT_HUB_RUN_MAX_TIMEOUT_SECS).contains(&self.timeout_secs)
        {
            return Err("agent_hub_run policy projection exceeds a required bound".into());
        }
        for root in self.workspaces.iter().chain(&self.context_roots) {
            validate_alias(&root.alias)?;
            validate_absolute_path(&root.path)?;
            if root.root_identity.is_empty() || root.root_identity.len() > 128 {
                return Err("agent_hub_run root identity exceeds a required bound".into());
            }
        }
        if self
            .workspaces
            .windows(2)
            .any(|items| items[0].alias >= items[1].alias)
            || self
                .context_roots
                .windows(2)
                .any(|items| items[0].alias >= items[1].alias)
        {
            return Err("agent_hub_run projected roots are not canonical".into());
        }
        if self.overlays.iter().chain(&self.capabilities).any(|item| {
            item.name.is_empty()
                || item.name.len() > 128
                || item.access.is_empty()
                || item.access.len() > 64
        }) || self.overlays.windows(2).any(|items| items[0] == items[1])
            || self
                .capabilities
                .windows(2)
                .any(|items| items[0] == items[1])
        {
            return Err("agent_hub_run capability projection is invalid".into());
        }
        validate_digest(&self.prompt_digest)?;
        if !self.image_id.starts_with("sha256:") {
            return Err("agent_hub_run image identity must be sha256".into());
        }
        validate_digest(&self.image_id[7..])?;
        if self.review.config_digest.is_empty()
            || self.review.config_digest.len() > 128
            || self.review.environment_bundle_digest.is_empty()
            || self.review.environment_bundle_digest.len() > 128
            || self.review.approval_bundle_digest.is_empty()
            || self.review.approval_bundle_digest.len() > 128
        {
            return Err("agent_hub_run review projection exceeds a required bound".into());
        }
        validate_alias(&self.review.primary_workspace)?;
        if let Some(path) = &self.review.workspace_export_root {
            validate_absolute_path(path)?;
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > AGENT_HUB_RUN_MAX_POLICY_PROJECTION_BYTES
        {
            return Err("agent_hub_run policy projection exceeds encoded limit".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunCanonicalArtifactV1 {
    pub version: u32,
    #[schemars(length(min = 64, max = 64))]
    pub digest: String,
    #[schemars(length(max = 699052))]
    pub canonical_bytes_base64: String,
}

impl AgentHubRunCanonicalArtifactV1 {
    fn encode(artifact: &AttendedAuthorizationArtifact) -> Result<Self, String> {
        if artifact.canonical_bytes.len() > AGENT_HUB_RUN_MAX_CANONICAL_BYTES {
            return Err("agent_hub_run canonical execution exceeds limit".into());
        }
        Ok(Self {
            version: artifact.version,
            digest: artifact.digest.clone(),
            canonical_bytes_base64: base64_encode(&artifact.canonical_bytes),
        })
    }

    pub fn decode(&self) -> Result<AttendedAuthorizationArtifact, String> {
        validate_digest(&self.digest)?;
        let canonical_bytes = base64_decode(&self.canonical_bytes_base64)?;
        if canonical_bytes.len() > AGENT_HUB_RUN_MAX_CANONICAL_BYTES {
            return Err("agent_hub_run canonical execution exceeds limit".into());
        }
        Ok(AttendedAuthorizationArtifact {
            version: self.version,
            digest: self.digest.clone(),
            canonical_bytes,
        })
    }
}

/// Exact canonical execution submitted to the gateway.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunResolvedV1 {
    pub schema: String,
    pub invocation_id: String,
    #[schemars(length(min = 64, max = 64))]
    pub canonical_digest: String,
    #[schemars(length(min = 64, max = 64))]
    pub request_digest: String,
    pub canonical_execution: AgentHubRunCanonicalArtifactV1,
    pub policy_projection: AgentHubRunPolicyProjectionV1,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl AgentHubRunResolvedV1 {
    pub fn new(
        invocation_id: String,
        canonical_execution: AttendedAuthorizationArtifact,
        requested_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Self, String> {
        let mut request = Self {
            schema: AGENT_HUB_RUN_RESOLVED_SCHEMA_V1.into(),
            invocation_id,
            canonical_digest: canonical_execution.digest.clone(),
            request_digest: String::new(),
            policy_projection: AgentHubRunPolicyProjectionV1::derive(&canonical_execution)?,
            canonical_execution: AgentHubRunCanonicalArtifactV1::encode(&canonical_execution)?,
            requested_at,
            expires_at,
        };
        request.request_digest = request.compute_digest()?;
        request.validate(requested_at)?;
        Ok(request)
    }

    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), String> {
        if self.schema != AGENT_HUB_RUN_RESOLVED_SCHEMA_V1 {
            return Err("unsupported agent_hub_run resolved schema".into());
        }
        let invocation_id = uuid::Uuid::parse_str(&self.invocation_id)
            .map_err(|_| "agent_hub_run invocation ID is not a UUID".to_string())?;
        if invocation_id.to_string() != self.invocation_id {
            return Err("agent_hub_run invocation ID is not canonical".into());
        }
        validate_digest(&self.canonical_digest)?;
        validate_digest(&self.request_digest)?;
        if self.requested_at > now || self.expires_at <= now || self.expires_at <= self.requested_at
        {
            return Err("agent_hub_run request timestamps are invalid or expired".into());
        }
        if self.canonical_execution.digest != self.canonical_digest {
            return Err("agent_hub_run canonical digest binding mismatch".into());
        }
        let artifact = self.canonical_execution.decode()?;
        artifact.verify_v3()?;
        let projection = AgentHubRunPolicyProjectionV1::derive(&artifact)?;
        if projection != self.policy_projection {
            return Err("agent_hub_run policy projection binding mismatch".into());
        }
        if self.compute_digest()? != self.request_digest {
            return Err("agent_hub_run request digest mismatch".into());
        }
        if serde_json::to_vec(&self.gateway_event()?)
            .map_err(|error| error.to_string())?
            .len()
            > AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES
        {
            return Err("agent_hub_run encoded gateway request exceeds limit".into());
        }
        Ok(())
    }

    fn compute_digest(&self) -> Result<String, String> {
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        value
            .as_object_mut()
            .expect("resolved request is object")
            .insert(
                "request_digest".into(),
                serde_json::Value::String(String::new()),
            );
        Ok(sha256_hex_with_domain(
            RESOLVED_DOMAIN,
            &canonical_json_bytes(value)?,
        ))
    }

    pub fn gateway_event(&self) -> Result<crate::OpenCodeHookInput, String> {
        let artifact = self.canonical_execution.decode()?;
        let execution = artifact.verify_v3()?;
        let value = serde_json::to_value(self).expect("resolved request serializes");
        let mut workspace_roots = execution
            .execution
            .workspaces
            .iter()
            .map(|workspace| workspace.host_path.clone())
            .chain(
                execution
                    .execution
                    .context_roots
                    .iter()
                    .map(|root| root.host_path.clone()),
            )
            .collect::<Vec<_>>();
        workspace_roots.sort();
        Ok(crate::OpenCodeHookInput {
            session_id: crate::SessionId::new(self.invocation_id.clone()),
            tool: crate::OpenCodeTool::Unknown(AGENT_HUB_RUN_PERMISSION_NAME.into()),
            tool_input: value.clone(),
            tool_match_input: Some(
                serde_json::to_value(&self.policy_projection)
                    .expect("policy projection serializes"),
            ),
            cwd: execution.execution.config_root,
            workspace_roots: Some(workspace_roots),
            hook_event_name: "agent_hub_run.host.before".into(),
            session_title: Some(format!("Agent Hub run {}", self.invocation_id)),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubRunDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubRunDecisionAuthority {
    Automatic,
    Human,
}

/// Versioned gateway decision. Every request binding is echoed and validated by
/// the host before the private receipt is created.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunDecisionV1 {
    pub schema: String,
    pub invocation_id: String,
    #[schemars(length(min = 64, max = 64))]
    pub canonical_digest: String,
    #[schemars(length(min = 64, max = 64))]
    pub request_digest: String,
    pub decision: AgentHubRunDecision,
    pub authority: AgentHubRunDecisionAuthority,
    pub approval_id: String,
    #[schemars(length(min = 1, max = 4096))]
    pub reason: Option<String>,
    pub decided_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl AgentHubRunDecisionV1 {
    pub fn validate(
        &self,
        request: &AgentHubRunResolvedV1,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.schema != AGENT_HUB_RUN_DECISION_SCHEMA_V1
            || self.invocation_id != request.invocation_id
            || self.canonical_digest != request.canonical_digest
            || self.request_digest != request.request_digest
            || self.expires_at != request.expires_at
        {
            return Err("agent_hub_run gateway decision binding mismatch".into());
        }
        let approval_id = uuid::Uuid::parse_str(&self.approval_id)
            .map_err(|_| "agent_hub_run approval ID is not a UUID".to_string())?;
        if approval_id.to_string() != self.approval_id {
            return Err("agent_hub_run approval ID is not canonical".into());
        }
        validate_digest(&self.canonical_digest)?;
        validate_digest(&self.request_digest)?;
        if self.decided_at < request.requested_at
            || self.decided_at > now
            || self.expires_at <= now
            || self.decided_at >= self.expires_at
            || self.reason.as_ref().is_some_and(|reason| {
                reason.is_empty() || reason.len() > AGENT_HUB_RUN_MAX_REASON_BYTES
            })
        {
            return Err("agent_hub_run gateway decision timestamps are invalid or expired".into());
        }
        if self.authority == AgentHubRunDecisionAuthority::Automatic
            && self.approval_id != request.invocation_id
        {
            return Err("agent_hub_run automatic decision approval ID is invalid".into());
        }
        Ok(())
    }
}

/// Authority-bound gateway decision plus a private integrity checksum. The
/// checksum is local integrity evidence, not a gateway signature.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunReceiptV1 {
    #[serde(flatten)]
    decision: AgentHubRunDecisionV1,
    pub checksum: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubRunApprovalReceiptStatus {
    Issued,
    Unavailable,
}

/// Stable approval evidence slot present on every one-step terminal envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunApprovalReceiptV1 {
    pub schema: String,
    pub status: AgentHubRunApprovalReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<AgentHubRunReceiptV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub error: Option<String>,
}

impl AgentHubRunApprovalReceiptV1 {
    pub fn issued(receipt: AgentHubRunReceiptV1) -> Self {
        Self {
            schema: AGENT_HUB_RUN_APPROVAL_RECEIPT_SCHEMA_V1.into(),
            status: AgentHubRunApprovalReceiptStatus::Issued,
            receipt: Some(receipt),
            error: None,
        }
    }

    pub fn unavailable(error: String) -> Self {
        Self {
            schema: AGENT_HUB_RUN_APPROVAL_RECEIPT_SCHEMA_V1.into(),
            status: AgentHubRunApprovalReceiptStatus::Unavailable,
            receipt: None,
            error: Some(error),
        }
    }
}

/// Plugin-consumable terminal envelope. `local_result_v2` is deliberately a
/// bounded JSON value so the CLI's established aggregate remains the source of
/// truth without coupling protocol compatibility to local storage internals.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunOutputV1 {
    pub schema: String,
    pub invocation_id: String,
    pub approval_receipt: AgentHubRunApprovalReceiptV1,
    pub local_result_v2: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub error: Option<String>,
}

impl AgentHubRunOutputV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != AGENT_HUB_RUN_OUTPUT_SCHEMA_V1
            || self.approval_receipt.schema != AGENT_HUB_RUN_APPROVAL_RECEIPT_SCHEMA_V1
            || !matches!(
                (
                    &self.approval_receipt.status,
                    &self.approval_receipt.receipt,
                    &self.approval_receipt.error
                ),
                (AgentHubRunApprovalReceiptStatus::Issued, Some(_), None)
                    | (AgentHubRunApprovalReceiptStatus::Unavailable, None, Some(_))
            )
            || uuid::Uuid::parse_str(&self.invocation_id)
                .ok()
                .is_none_or(|id| id.to_string() != self.invocation_id)
            || self
                .local_result_v2
                .get("schema")
                .and_then(serde_json::Value::as_str)
                != Some("local_result_v2")
            || self.error.as_ref().is_some_and(|error| {
                error.is_empty() || error.len() > AGENT_HUB_RUN_MAX_REASON_BYTES
            })
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len()
                > AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES
        {
            return Err("invalid or oversized agent_hub_run output envelope".into());
        }
        Ok(())
    }
}

impl AgentHubRunReceiptV1 {
    pub fn issue(
        request: &AgentHubRunResolvedV1,
        decision: AgentHubRunDecisionV1,
        now: DateTime<Utc>,
    ) -> Result<Self, String> {
        decision.validate(request, now)?;
        let checksum = sha256_hex_with_domain(
            RECEIPT_DOMAIN,
            &canonical_json_bytes(serde_json::to_value(&decision).map_err(|e| e.to_string())?)?,
        );
        Ok(Self { decision, checksum })
    }
    pub fn verify(
        &self,
        request: &AgentHubRunResolvedV1,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.decision.validate(request, now)?;
        self.verify_binding(&request.invocation_id, &request.canonical_digest, now)
    }
    pub fn verify_binding(
        &self,
        invocation_id: &str,
        canonical_digest: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.decision.schema != AGENT_HUB_RUN_DECISION_SCHEMA_V1
            || self.decision.invocation_id != invocation_id
            || self.decision.canonical_digest != canonical_digest
            || self.decision.decided_at > now
            || self.decision.expires_at <= now
        {
            return Err("agent_hub_run receipt binding mismatch or expired".into());
        }
        let invocation_id = uuid::Uuid::parse_str(&self.decision.invocation_id)
            .map_err(|_| "agent_hub_run invocation ID is not a UUID".to_string())?;
        if invocation_id.to_string() != self.decision.invocation_id {
            return Err("agent_hub_run invocation ID is not canonical".into());
        }
        let approval_id = uuid::Uuid::parse_str(&self.decision.approval_id)
            .map_err(|_| "agent_hub_run approval ID is not a UUID".to_string())?;
        if approval_id.to_string() != self.decision.approval_id {
            return Err("agent_hub_run approval ID is not canonical".into());
        }
        validate_digest(&self.decision.canonical_digest)?;
        validate_digest(&self.decision.request_digest)?;
        if self.decision.decided_at >= self.decision.expires_at
            || self.decision.reason.as_ref().is_some_and(|reason| {
                reason.is_empty() || reason.len() > AGENT_HUB_RUN_MAX_REASON_BYTES
            })
        {
            return Err("agent_hub_run receipt decision fields are invalid".into());
        }
        if self.decision.authority == AgentHubRunDecisionAuthority::Automatic
            && self.decision.approval_id != self.decision.invocation_id
        {
            return Err("agent_hub_run automatic receipt approval ID is invalid".into());
        }
        let expected = sha256_hex_with_domain(
            RECEIPT_DOMAIN,
            &canonical_json_bytes(
                serde_json::to_value(&self.decision).map_err(|e| e.to_string())?,
            )?,
        );
        if self.checksum != expected {
            return Err("agent_hub_run receipt checksum mismatch".into());
        }
        Ok(())
    }
    pub fn decision_record(&self) -> &AgentHubRunDecisionV1 {
        &self.decision
    }
    pub fn receipt_id(&self) -> &str {
        &self.decision.approval_id
    }
    pub fn invocation_id(&self) -> &str {
        &self.decision.invocation_id
    }
    pub fn canonical_digest(&self) -> &str {
        &self.decision.canonical_digest
    }
    pub fn decision(&self) -> AgentHubRunDecision {
        self.decision.decision
    }
    pub fn authority(&self) -> AgentHubRunDecisionAuthority {
        self.decision.authority
    }
    pub fn reason(&self) -> Option<&str> {
        self.decision.reason.as_deref()
    }
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.decision.expires_at
    }
}

fn validate_alias(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        return Err("invalid agent_hub_run alias".into());
    }
    Ok(())
}
fn validate_absolute_path(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > AGENT_HUB_RUN_MAX_PATH_BYTES
        || value.contains('\0')
        || !std::path::Path::new(value).is_absolute()
    {
        return Err("agent_hub_run path must be a bounded absolute path".into());
    }
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn base64_decode(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(4)
        || value.len() > AGENT_HUB_RUN_MAX_CANONICAL_BYTES.div_ceil(3) * 4
    {
        return Err("agent_hub_run canonical base64 is invalid or exceeds limit".into());
    }
    let decode = |byte: u8| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut output = Vec::with_capacity(value.len() / 4 * 3);
    for (index, chunk) in value.as_bytes().chunks(4).enumerate() {
        let last = index + 1 == value.len() / 4;
        if chunk[0] == b'=' || chunk[1] == b'=' || (!last && (chunk[2] == b'=' || chunk[3] == b'='))
        {
            return Err("agent_hub_run canonical base64 padding is invalid".into());
        }
        let a = decode(chunk[0])
            .ok_or_else(|| "agent_hub_run canonical base64 is invalid".to_string())?;
        let b = decode(chunk[1])
            .ok_or_else(|| "agent_hub_run canonical base64 is invalid".to_string())?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            decode(chunk[2])
                .ok_or_else(|| "agent_hub_run canonical base64 is invalid".to_string())?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            decode(chunk[3])
                .ok_or_else(|| "agent_hub_run canonical base64 is invalid".to_string())?
        };
        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err("agent_hub_run canonical base64 padding is invalid".into());
        }
        let decoded =
            (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
        output.push((decoded >> 16) as u8);
        if chunk[2] != b'=' {
            output.push((decoded >> 8) as u8);
        }
        if chunk[3] != b'=' {
            output.push(decoded as u8);
        }
    }
    if base64_encode(&output) != value {
        return Err("agent_hub_run canonical base64 is not canonical".into());
    }
    Ok(output)
}
fn validate_digest(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("agent_hub_run digest must be lowercase SHA-256".into());
    }
    Ok(())
}
fn canonical_json_bytes(value: serde_json::Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(overlays: Vec<String>, timeout_secs: u64) -> AgentHubRunIntentV1 {
        AgentHubRunIntentV1 {
            schema: AgentHubRunIntentSchema::V1,
            environment: "test".into(),
            image_family: AgentHubRunImageFamily::JobCode,
            network_class: AgentHubRunNetworkClass::None,
            network_resources: Vec::new(),
            tool_approval: AgentHubRunToolApproval::Auto,
            overlays,
            timeout_secs,
            prompt: "test".into(),
            workspaces: vec![AgentHubRunWorkspaceV1 {
                alias: "primary".into(),
                path: "/workspace".into(),
                access: WorkspaceAccess::ReadWrite,
            }],
            context_roots: Vec::new(),
            primary_workspace: "primary".into(),
            workspace_export_root: None,
        }
    }

    #[test]
    fn runtime_overlay_name_count_and_timeout_boundaries_are_exact() {
        assert!(
            intent(vec!["x".repeat(AGENT_HUB_RUN_MAX_OVERLAY_NAME_BYTES)], 1)
                .validate_shape()
                .is_ok()
        );
        assert!(
            intent(
                vec!["x".repeat(AGENT_HUB_RUN_MAX_OVERLAY_NAME_BYTES + 1)],
                1
            )
            .validate_shape()
            .is_err()
        );
        let overlays = (0..AGENT_HUB_RUN_MAX_SELECTORS)
            .map(|index| format!("overlay-{index}"))
            .collect::<Vec<_>>();
        assert!(
            intent(overlays.clone(), AGENT_HUB_RUN_MAX_TIMEOUT_SECS)
                .validate_shape()
                .is_ok()
        );
        let mut too_many = overlays;
        too_many.push("extra".into());
        assert!(
            intent(too_many, AGENT_HUB_RUN_MAX_TIMEOUT_SECS)
                .validate_shape()
                .is_err()
        );
        assert!(intent(Vec::new(), 0).validate_shape().is_err());
        assert!(
            intent(Vec::new(), AGENT_HUB_RUN_MAX_TIMEOUT_SECS + 1)
                .validate_shape()
                .is_err()
        );
    }

    #[test]
    fn generated_schema_matches_runtime_string_count_digest_and_timeout_bounds() {
        let intent_schema =
            serde_json::to_value(schemars::schema_for!(AgentHubRunIntentV1)).unwrap();
        assert_eq!(
            intent_schema["properties"]["overlays"]["maxItems"],
            AGENT_HUB_RUN_MAX_SELECTORS
        );
        assert_eq!(
            intent_schema["properties"]["overlays"]["items"]["minLength"],
            1
        );
        assert_eq!(
            intent_schema["properties"]["overlays"]["items"]["maxLength"],
            AGENT_HUB_RUN_MAX_OVERLAY_NAME_BYTES
        );
        assert_eq!(intent_schema["properties"]["timeout_secs"]["minimum"], 1);
        assert_eq!(
            intent_schema["properties"]["timeout_secs"]["maximum"],
            AGENT_HUB_RUN_MAX_TIMEOUT_SECS
        );

        let decision = serde_json::to_value(schemars::schema_for!(AgentHubRunDecisionV1)).unwrap();
        for field in ["canonical_digest", "request_digest"] {
            assert_eq!(decision["properties"][field]["minLength"], 64);
            assert_eq!(decision["properties"][field]["maxLength"], 64);
        }
        assert_eq!(decision["properties"]["reason"]["minLength"], 1);
        assert_eq!(
            decision["properties"]["reason"]["maxLength"],
            AGENT_HUB_RUN_MAX_REASON_BYTES
        );

        let output = serde_json::to_value(schemars::schema_for!(AgentHubRunOutputV1)).unwrap();
        assert_eq!(output["properties"]["error"]["minLength"], 1);
        assert_eq!(
            output["properties"]["error"]["maxLength"],
            AGENT_HUB_RUN_MAX_REASON_BYTES
        );
    }
}

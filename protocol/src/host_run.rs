//! Canonical protocol for a host-owned, one-step attended run.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AttendedAuthorizationArtifact, NetworkResourceKind, WorkspaceAccess, sha256_hex_with_domain,
};

pub const AGENT_HUB_RUN_PERMISSION_NAME: &str = "agent_hub_run_resolved";
pub const AGENT_HUB_RUN_INTENT_SCHEMA_V1: &str = "agent_hub_run_intent_v1";
pub const AGENT_HUB_RUN_RESOLVED_SCHEMA_V1: &str = "agent_hub_run_resolved_v1";
pub const AGENT_HUB_RUN_DECISION_SCHEMA_V1: &str = "agent_hub_run_decision_v1";
pub const AGENT_HUB_RUN_MAX_PROMPT_BYTES: usize = 1_000_000;
pub const AGENT_HUB_RUN_MAX_MOUNTS: usize = 32;
pub const AGENT_HUB_RUN_MAX_SELECTORS: usize = 32;
pub const AGENT_HUB_RUN_MAX_TIMEOUT_SECS: u64 = 86_400;
pub const AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES: usize = 1024 * 1024;
pub const AGENT_HUB_RUN_MAX_GATEWAY_OUTPUT_BYTES: usize = 64 * 1024;
pub const AGENT_HUB_RUN_MAX_REASON_BYTES: usize = 4096;
pub const AGENT_HUB_RUN_MAX_APPROVAL_TIMEOUT_SECS: u64 = 600;
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
    #[schemars(length(max = 32))]
    pub network_resources: Vec<NetworkResourceKind>,
    pub tool_approval: AgentHubRunToolApproval,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub overlays: Vec<String>,
    #[schemars(range(min = 1, max = 86400))]
    pub timeout_secs: u64,
    #[schemars(length(min = 1, max = 1000000))]
    pub prompt: String,
    #[schemars(length(min = 1, max = 32))]
    pub workspaces: Vec<AgentHubRunWorkspaceV1>,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub context_roots: Vec<AgentHubRunContextRootV1>,
    #[schemars(length(min = 1, max = 63))]
    pub primary_workspace: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AgentHubRunIntentSchema {
    #[serde(rename = "agent_hub_run_intent_v1")]
    V1,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AgentHubRunImageFamily {
    #[serde(rename = "job-code")]
    JobCode,
}

impl AgentHubRunImageFamily {
    pub fn as_str(self) -> &'static str {
        "job-code"
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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
    #[schemars(length(min = 1, max = 4096))]
    pub path: String,
    pub access: WorkspaceAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunContextRootV1 {
    #[schemars(length(min = 1, max = 63))]
    pub alias: String,
    #[schemars(length(min = 1, max = 4096))]
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

/// Exact canonical execution submitted to the gateway.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubRunResolvedV1 {
    pub schema: String,
    pub invocation_id: String,
    pub canonical_digest: String,
    pub request_digest: String,
    pub canonical_execution: AttendedAuthorizationArtifact,
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
            canonical_execution,
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
        self.canonical_execution.verify_v3()?;
        if self.compute_digest()? != self.request_digest {
            return Err("agent_hub_run request digest mismatch".into());
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
        let execution = self.canonical_execution.verify_v3()?;
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
            tool_match_input: Some(value),
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
    pub canonical_digest: String,
    pub request_digest: String,
    pub decision: AgentHubRunDecision,
    pub authority: AgentHubRunDecisionAuthority,
    pub approval_id: String,
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
            || self
                .reason
                .as_ref()
                .is_some_and(|reason| reason.len() > AGENT_HUB_RUN_MAX_REASON_BYTES)
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
            || self
                .decision
                .reason
                .as_ref()
                .is_some_and(|reason| reason.len() > AGENT_HUB_RUN_MAX_REASON_BYTES)
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
        || value.len() > 4096
        || value.contains('\0')
        || !std::path::Path::new(value).is_absolute()
    {
        return Err("agent_hub_run path must be a bounded absolute path".into());
    }
    Ok(())
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

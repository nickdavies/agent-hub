use std::fmt;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};
use uuid::Uuid;

use crate::sessions::SessionId;
use crate::tool::Tool;

// ===========================================================================
// ExtraContext — typed review artifacts attached to approval requests
// ===========================================================================

/// Structured context attached to an approval request for human review.
///
/// - `Diff` — a unified diff showing what a file-write tool would change.
/// - `DippyReason` — the delegate subprocess's reasoning for escalating a shell command.
///
/// Serialized with `#[serde(untagged)]` so the wire format stays flat:
/// `{"diff": "..."}` or `{"dippy_reason": "..."}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ExtraContext {
    Diff { diff: String },
    DippyReason { dippy_reason: String },
}

impl fmt::Display for ExtraContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtraContext::Diff { diff } => f.write_str(diff),
            ExtraContext::DippyReason { dippy_reason } => f.write_str(dippy_reason),
        }
    }
}

// ===========================================================================
// RequestType — what kind of approval request this is
// ===========================================================================

/// The type of approval request.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestType {
    ToolUse,
    /// A plan-mode question proxied from opencode's question tool.
    PlanQuestion,
    /// A request from a non-agent source (e.g. assistant-pipelines), named by
    /// `provider`. Its content is confidential: it is shown only on the
    /// authenticated approval page, never in notifications or session status,
    /// is not persisted, and is dropped from memory once resolved. It may carry
    /// a [`DisplaySpec`].
    External,
}

impl RequestType {
    pub fn is_external(self) -> bool {
        self == RequestType::External
    }
}

// ===========================================================================
// DisplaySpec — caller-provided presentation for external requests
// ===========================================================================

/// How to present an external request's `tool_input` for review.
///
/// It controls presentation only, never values: every field key must exist in
/// `tool_input`, and keys not listed are still shown after the listed ones. What
/// the reviewer sees is therefore always the input that gets approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DisplaySpec {
    /// Heading shown above the fields. Must come from the caller's code (e.g.
    /// the action type), never from model-written content.
    #[serde(default)]
    pub heading: Option<String>,
    pub fields: Vec<DisplayField>,
    /// `tool_input` keys whose values, in order, form the one-line summary in
    /// the dashboard queue. Empty means the default summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
}

/// One `tool_input` field, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DisplayField {
    /// Top-level key in `tool_input`.
    pub key: String,
    /// Label to show instead of the key.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub format: FieldFormat,
}

/// How to render a field's value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldFormat {
    #[default]
    Text,
    /// Preserve line breaks.
    Multiline,
    /// An RFC 3339 timestamp, shown in local time alongside the raw value.
    Datetime,
    /// An array, shown one item per line.
    List,
}

impl DisplaySpec {
    /// Check that every field and summary key refers to a top-level key of
    /// `tool_input`.
    pub fn validate(&self, tool_input: &serde_json::Value) -> Result<(), String> {
        let obj = tool_input
            .as_object()
            .ok_or("display requires tool_input to be an object")?;
        for field in &self.fields {
            if !obj.contains_key(&field.key) {
                return Err(format!("display field '{}' not in tool_input", field.key));
            }
        }
        for key in &self.summary {
            if !obj.contains_key(key) {
                return Err(format!("display summary key '{key}' not in tool_input"));
            }
        }
        Ok(())
    }
}

// ===========================================================================
// HookEventName — which provider hook event triggered the request
// ===========================================================================

/// Provider hook event name (e.g. "PreToolUse", "preToolUse", "tool.execute.before").
///
/// Known variants are strongly typed; unknown provider events are captured as
/// `Other(String)` so the system is forward-compatible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HookEventName {
    Known(KnownHookEvent),
    Other(String),
}

/// The known hook event names across all providers.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Display, EnumString,
)]
pub enum KnownHookEvent {
    /// Claude Code's pre-tool-use event.
    PreToolUse,
    /// Claude Code's permission-request event (camelCase alias handled via From).
    PermissionRequest,
    /// Cursor's pre-tool-use event (camelCase alias).
    #[serde(rename = "preToolUse")]
    #[strum(serialize = "preToolUse")]
    PreToolUseCamel,
    /// OpenCode's tool-execute event.
    #[serde(rename = "tool.execute.before")]
    #[strum(serialize = "tool.execute.before")]
    ToolExecuteBefore,
    /// OpenCode's permission-ask event.
    #[serde(rename = "permission.ask")]
    #[strum(serialize = "permission.ask")]
    PermissionAsk,
}

impl HookEventName {
    /// Returns the string representation of the event name.
    pub fn as_str(&self) -> &str {
        match self {
            HookEventName::Known(k) => match k {
                KnownHookEvent::PreToolUse => "PreToolUse",
                KnownHookEvent::PermissionRequest => "PermissionRequest",
                KnownHookEvent::PreToolUseCamel => "preToolUse",
                KnownHookEvent::ToolExecuteBefore => "tool.execute.before",
                KnownHookEvent::PermissionAsk => "permission.ask",
            },
            HookEventName::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for HookEventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for HookEventName {
    fn from(s: String) -> Self {
        match s.as_str() {
            "PreToolUse" => HookEventName::Known(KnownHookEvent::PreToolUse),
            "PermissionRequest" => HookEventName::Known(KnownHookEvent::PermissionRequest),
            "preToolUse" => HookEventName::Known(KnownHookEvent::PreToolUseCamel),
            "tool.execute.before" => HookEventName::Known(KnownHookEvent::ToolExecuteBefore),
            "permission.ask" => HookEventName::Known(KnownHookEvent::PermissionAsk),
            _ => HookEventName::Other(s),
        }
    }
}

// ===========================================================================
// ApprovalContext
// ===========================================================================

/// Contextual information attached to an approval request.
///
/// Shared between the gateway (which constructs it), the server (which stores it),
/// and the CLI (which displays it).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalContext {
    /// Workspace roots known to the provider at hook time.
    pub workspace_roots: Vec<String>,
    /// The provider hook event name (e.g. "PreToolUse", "preToolUse", "tool.execute.before").
    pub hook_event_name: HookEventName,
    /// Computed review artifacts: diffs for file-write tools, delegate reasoning, etc.
    pub extra: Option<ExtraContext>,
}

/// A full approval record as stored by the server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Approval {
    pub id: Uuid,
    pub request_id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub project: String,
    #[serde(rename = "tool_name")]
    pub tool: Tool,
    /// Tool arguments — genuinely polymorphic across tools (Bash has `command`,
    /// Write has `path`+`content`, etc.).
    pub tool_input: serde_json::Value,
    /// Provider that originated this approval request (e.g. "claude-code", "cursor", "opencode").
    pub provider: String,
    /// Request classification, including confidential external requests.
    pub request_type: RequestType,
    pub context: ApprovalContext,
    /// Presentation for external requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplaySpec>,
    pub created_at: DateTime<Utc>,
    pub status: ApprovalStatus,
}

/// Tagged status of an approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved { message: Option<String> },
    Denied { reason: String },
    Cancelled,
}

impl ApprovalStatus {
    pub fn is_resolved(&self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }
}

// ---------------------------------------------------------------------------
// Request / response types for the approval HTTP API
// ---------------------------------------------------------------------------

/// POST /api/v1/hooks/approval — request body sent by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub cwd: String,
    #[serde(rename = "tool_name")]
    pub tool: Tool,
    /// Tool arguments — genuinely polymorphic (see `Approval::tool_input`).
    pub tool_input: serde_json::Value,
    /// Provider identifier: "claude-code" | "cursor" | "opencode"
    pub provider: String,
    /// Request classification; external content has restricted retention and notifications.
    pub request_type: RequestType,
    pub context: ApprovalContext,
    /// Presentation hints; only allowed when `request_type` is "external".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplaySpec>,
}

/// POST /api/v1/hooks/approval — response from the server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalResponse {
    pub id: Uuid,
    #[serde(flatten)]
    pub status: ApprovalStatus,
}

/// GET /api/v1/approvals/{id}/wait — long-poll response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalWaitResponse {
    #[serde(flatten)]
    pub status: ApprovalStatus,
}

/// POST /api/v1/approvals/{id}/resolve — request body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalResolveRequest {
    pub decision: ApprovalDecision,
    pub message: Option<String>,
}

/// Decision sent when resolving an approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
    Cancel,
}

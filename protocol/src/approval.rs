use std::fmt;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};
use uuid::Uuid;

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
///
/// Currently only `ToolUse` exists; `PlanQuestion` is planned for Phase 2.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RequestType {
    ToolUse,
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
    pub session_id: String,
    pub session_display_name: String,
    pub project: String,
    #[serde(rename = "tool_name")]
    pub tool: Tool,
    /// Tool arguments — genuinely polymorphic across tools (Bash has `command`,
    /// Write has `path`+`content`, etc.).
    pub tool_input: serde_json::Value,
    /// Provider that originated this approval request (e.g. "claude-code", "cursor", "opencode").
    pub provider: String,
    /// Request type; currently always "tool_use". "plan_question" is Phase 2.
    pub request_type: RequestType,
    pub context: ApprovalContext,
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
    pub session_id: String,
    pub session_display_name: String,
    pub cwd: String,
    #[serde(rename = "tool_name")]
    pub tool: Tool,
    /// Tool arguments — genuinely polymorphic (see `Approval::tool_input`).
    pub tool_input: serde_json::Value,
    /// Provider identifier: "claude-code" | "cursor" | "opencode"
    pub provider: String,
    /// Request type: "tool_use" (Phase 2 will add "plan_question")
    pub request_type: RequestType,
    pub context: ApprovalContext,
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

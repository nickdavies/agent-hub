//! Gateway-local types.
//!
//! Contains the canonical hook event representation, decision/output types,
//! and typed tool input structs used throughout the gateway's approval flow.
//! None of these types are shared with other crates.

use config::ConfigDecision;
use protocol::{SessionId, ToolCall};

// ===========================================================================
// Canonical hook event
// ===========================================================================

/// Internal canonical representation of a hook input event.
/// All provider-specific wire formats are parsed into this before any rule evaluation.
pub struct ToolHookEvent {
    pub session_id: SessionId,
    pub session_display_name: String,
    pub tool_call: ToolCall,
    pub cwd: String,
    pub workspace_roots: Vec<String>,
    pub hook_event_name: String,
}

impl From<&ToolHookEvent> for protocol::DelegatePayload {
    fn from(event: &ToolHookEvent) -> Self {
        Self {
            tool: event.tool_call.tool(),
            tool_input: event.tool_call.raw_input().clone(),
            cwd: event.cwd.clone(),
            hook_event_name: event.hook_event_name.clone(),
        }
    }
}

// ===========================================================================
// Parse error
// ===========================================================================

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

// ===========================================================================
// Intermediate decision (pre-resolution)
// ===========================================================================

/// The three possible outcomes after config evaluation + optional delegation,
/// before the gateway acts on them (escalate to server, format output, etc.).
///
/// Unlike `ConfigDecision`, `Ask` carries an optional reason from the delegate
/// subprocess (e.g. Dippy's analysis) that is forwarded to the human reviewer.
#[derive(Debug)]
pub enum HookDecision {
    Allow,
    Deny(Option<String>),
    Ask(Option<String>),
}

impl From<ConfigDecision> for HookDecision {
    fn from(d: ConfigDecision) -> Self {
        match d {
            ConfigDecision::Allow => HookDecision::Allow,
            ConfigDecision::Deny(reason) => HookDecision::Deny(reason),
            ConfigDecision::Ask => HookDecision::Ask(None),
        }
    }
}

impl From<protocol::DelegateOutput> for HookDecision {
    fn from(output: protocol::DelegateOutput) -> Self {
        let inner = output.hook_specific_output;
        match inner.permission_decision {
            protocol::DelegatePermission::Allow => HookDecision::Allow,
            protocol::DelegatePermission::Deny => {
                HookDecision::Deny(inner.permission_decision_reason)
            }
            protocol::DelegatePermission::Ask => {
                HookDecision::Ask(inner.permission_decision_reason)
            }
        }
    }
}

// ===========================================================================
// Final output
// ===========================================================================

/// Status of the final approval decision sent back to the provider.
#[derive(Debug, Clone)]
pub enum DecisionStatus {
    Approved,
    Denied,
    DeniedWithReason(String),
    TimedOut,
    PipelineError,
}

pub const APPROVAL_TIMEOUT_MESSAGE: &str = "Your approval request timed out because the operator was away. This is not a denial. Feel free to retry if you still need this.";
pub const APPROVAL_PIPELINE_ERROR_MESSAGE: &str = "There was an error in the approvals pipeline. This is a null answer: it is neither an approval nor a rejection of your intended action. Do not take this as a signal that your intended path is unwanted. Try again; if the error recurs, abort and report it to the operator.";

/// The final decision produced by the gateway's approval flow.
///
/// This is what provider `format_output` functions consume to build their
/// wire-format JSON response.
#[derive(Debug, Clone)]
pub struct HookOutput {
    pub status: DecisionStatus,
    /// Optional human-readable message from the reviewer (for Approved decisions)
    /// or additional context.
    pub message: Option<String>,
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Build a human-readable display name from session ID and workspace roots.
pub(crate) fn build_display_name(session_id: &SessionId, roots: &[String]) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let normalize = |p: &str| -> String {
        if !home.is_empty() && p.starts_with(&home) {
            format!("~{}", &p[home.len()..])
        } else {
            p.to_string()
        }
    };

    let id_str = session_id.as_str();
    let id_part = &id_str[..id_str.len().min(8)];

    if roots.is_empty() {
        return format!("[{id_part}]");
    }

    let paths = roots
        .iter()
        .map(|r| normalize(r))
        .collect::<Vec<_>>()
        .join(", ");

    format!("{paths} [{id_part}]")
}

mod rules;
mod tools;

pub use rules::{
    DefaultAction, ResolvedAction, RuleAction, ToolConfig, default_to_resolved, load_tool_config,
    resolve_action,
};
pub use tools::{
    TOOL_DEFS, ToolCategory, ToolDef, expand_tool_group, find_tool_def, get_matchable_args,
    is_in_workspace, is_path_tool, normalise_tool_name, tools_in_category,
};

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

// --- Canonical hook event representation ---

/// Internal canonical representation of a hook input event.
/// All provider-specific wire formats are parsed into this before any rule evaluation.
pub struct ToolHookEvent {
    pub session_id: String,
    pub session_display_name: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub cwd: String,
    pub workspace_roots: Vec<String>,
    pub hook_event_name: String,
}

// --- Decision types ---

#[derive(Debug, Clone)]
pub enum DecisionStatus {
    Approved,
    Denied,
    DeniedWithReason(String),
}

#[derive(Debug, Clone)]
pub struct HookDecision {
    pub status: DecisionStatus,
    pub message: Option<String>,
}

// --- Approval context (sent to server with escalated requests) ---

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovalContext {
    /// Workspace roots known to the provider at hook time.
    pub workspace_roots: Vec<String>,
    /// The provider hook event name (e.g. "PreToolUse", "preToolUse", "tool.execute.before").
    pub hook_event_name: String,
    /// Per-tool opaque blob; rendered as JSON in the web UI, diff view for FileWrite tools.
    pub extra: Option<serde_json::Value>,
}

// --- Parse error ---

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

// --- Provider capabilities and trait ---

pub struct ProviderCapabilities {
    /// Hook can return approve/deny inline and block the agent.
    pub inline_approval: bool,
    /// Agent shows its own approval UI before the hook fires (informational only).
    pub agent_ui_prompt: bool,
    /// Provider exposes plan-mode questions through a hookable surface.
    pub plan_questions: bool,
    /// Hook payload carries rich conversation context beyond tool args.
    pub rich_context: bool,
}

pub trait Provider: Send + Sync + 'static {
    /// Short identifier used on the --flag and in log/server output.
    fn name(&self) -> &'static str;

    /// Parse the raw stdin JSON into a canonical ToolHookEvent.
    /// Also responsible for normalising tool names via the tool registry.
    fn parse_input(&self, raw: &str) -> Result<ToolHookEvent, ParseError>;

    /// Serialise a canonical HookDecision into the provider's wire format (stdout JSON).
    fn format_output(&self, event: &ToolHookEvent, decision: HookDecision) -> String;

    fn capabilities(&self) -> &ProviderCapabilities;
}

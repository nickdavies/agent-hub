//! Provider-specific wire-format types for the gateway.
//!
//! These types live in protocol (rather than in the gateway crate) so that:
//! 1. JSON schemas can be generated for TypeScript plugin consumption.
//! 2. The `capabilities` crate can implement `TryFrom<OpenCodeApprovalInput> for ToolHookEvent`.
//!
//! Each provider has an input type (what the hook receives on stdin) and one or
//! more output types (what the hook writes to stdout).
//!
//! Provider-specific tool enums (`OpenCodeTool`, `ClaudeTool`, `CursorTool`) handle
//! deserialization of provider-native tool names. `From<ProviderTool> for Tool`
//! impls normalize them to the canonical `Tool` enum.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use strum::{Display, EnumString};

use crate::tool::Tool;

// ===========================================================================
// PermissionDecision — allow/deny for provider hook output fields
// ===========================================================================

/// Binary permission decision used in provider output wire formats.
///
/// This replaces bare `String` fields that previously held "allow" or "deny".
/// Note: for delegate subprocess output, `DelegatePermission` adds an `Ask` variant.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Display, EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
}

// ===========================================================================
// Provider-specific tool enums
// ===========================================================================

// ---------------------------------------------------------------------------
// OpenCode tools
// ---------------------------------------------------------------------------

/// Tool names as sent by OpenCode.
///
/// OpenCode uses lowercase names and has some different names for tools
/// (e.g. `"edit"` maps to the canonical `Write` tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenCodeTool {
    Bash,
    Edit, // maps to Tool::Write
    Glob,
    Grep,
    MultiEdit,
    Read,
    Task,
    TodoWrite,
    WebFetch,
    Write,
    Unknown(String),
}

impl OpenCodeTool {
    fn as_wire_str(&self) -> &str {
        match self {
            OpenCodeTool::Bash => "bash",
            OpenCodeTool::Edit => "edit",
            OpenCodeTool::Glob => "glob",
            OpenCodeTool::Grep => "grep",
            OpenCodeTool::MultiEdit => "multiedit",
            OpenCodeTool::Read => "read",
            OpenCodeTool::Task => "task",
            OpenCodeTool::TodoWrite => "todowrite",
            OpenCodeTool::WebFetch => "webfetch",
            OpenCodeTool::Write => "write",
            OpenCodeTool::Unknown(s) => s.as_str(),
        }
    }

    fn from_wire(s: &str) -> OpenCodeTool {
        match s {
            "bash" => OpenCodeTool::Bash,
            "edit" => OpenCodeTool::Edit,
            "glob" => OpenCodeTool::Glob,
            "grep" => OpenCodeTool::Grep,
            "multiedit" => OpenCodeTool::MultiEdit,
            "read" => OpenCodeTool::Read,
            "task" => OpenCodeTool::Task,
            "todowrite" => OpenCodeTool::TodoWrite,
            "webfetch" => OpenCodeTool::WebFetch,
            "write" => OpenCodeTool::Write,
            // OpenCode also sends PascalCase names for tool.execute.before events,
            // so fall through to canonical parsing for those.
            other => match other {
                "Bash" => OpenCodeTool::Bash,
                "Glob" => OpenCodeTool::Glob,
                "Grep" => OpenCodeTool::Grep,
                "MultiEdit" => OpenCodeTool::MultiEdit,
                "Read" => OpenCodeTool::Read,
                "Task" => OpenCodeTool::Task,
                "TodoWrite" => OpenCodeTool::TodoWrite,
                "WebFetch" => OpenCodeTool::WebFetch,
                "Write" => OpenCodeTool::Write,
                "Edit" => OpenCodeTool::Edit,
                _ => OpenCodeTool::Unknown(other.to_owned()),
            },
        }
    }
}

impl From<OpenCodeTool> for Tool {
    fn from(t: OpenCodeTool) -> Self {
        match t {
            OpenCodeTool::Bash => Tool::Bash,
            OpenCodeTool::Edit => Tool::Write, // opencode "edit" = canonical "Write"
            OpenCodeTool::Glob => Tool::Glob,
            OpenCodeTool::Grep => Tool::Grep,
            OpenCodeTool::MultiEdit => Tool::MultiEdit,
            OpenCodeTool::Read => Tool::Read,
            OpenCodeTool::Task => Tool::Task,
            OpenCodeTool::TodoWrite => Tool::TodoWrite,
            OpenCodeTool::WebFetch => Tool::WebFetch,
            OpenCodeTool::Write => Tool::Write,
            OpenCodeTool::Unknown(s) => Tool::Unknown(s),
        }
    }
}

impl Serialize for OpenCodeTool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire_str())
    }
}

impl<'de> Deserialize<'de> for OpenCodeTool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(OpenCodeTool::from_wire(&s))
    }
}

impl fmt::Display for OpenCodeTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

impl JsonSchema for OpenCodeTool {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OpenCodeTool".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Tool name as sent by OpenCode (lowercase). Known: bash, edit, glob, grep, multiedit, read, task, todowrite, webfetch, write."
        })
    }
}

// ---------------------------------------------------------------------------
// Claude Code tools
// ---------------------------------------------------------------------------

/// Tool names as sent by Claude Code (PascalCase — same as canonical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeTool {
    Read,
    Grep,
    Glob,
    SemanticSearch,
    Write,
    StrReplace,
    Delete,
    Edit,
    MultiEdit,
    EditNotebook,
    NotebookEdit,
    Bash,
    Task,
    TodoWrite,
    WebFetch,
    Unknown(String),
}

impl ClaudeTool {
    fn as_wire_str(&self) -> &str {
        match self {
            ClaudeTool::Read => "Read",
            ClaudeTool::Grep => "Grep",
            ClaudeTool::Glob => "Glob",
            ClaudeTool::SemanticSearch => "SemanticSearch",
            ClaudeTool::Write => "Write",
            ClaudeTool::StrReplace => "StrReplace",
            ClaudeTool::Delete => "Delete",
            ClaudeTool::Edit => "Edit",
            ClaudeTool::MultiEdit => "MultiEdit",
            ClaudeTool::EditNotebook => "EditNotebook",
            ClaudeTool::NotebookEdit => "NotebookEdit",
            ClaudeTool::Bash => "Bash",
            ClaudeTool::Task => "Task",
            ClaudeTool::TodoWrite => "TodoWrite",
            ClaudeTool::WebFetch => "WebFetch",
            ClaudeTool::Unknown(s) => s.as_str(),
        }
    }

    fn from_wire(s: &str) -> ClaudeTool {
        match s {
            "Read" => ClaudeTool::Read,
            "Grep" => ClaudeTool::Grep,
            "Glob" => ClaudeTool::Glob,
            "SemanticSearch" => ClaudeTool::SemanticSearch,
            "Write" => ClaudeTool::Write,
            "StrReplace" => ClaudeTool::StrReplace,
            "Delete" => ClaudeTool::Delete,
            "Edit" => ClaudeTool::Edit,
            "MultiEdit" => ClaudeTool::MultiEdit,
            "EditNotebook" => ClaudeTool::EditNotebook,
            "NotebookEdit" => ClaudeTool::NotebookEdit,
            "Bash" => ClaudeTool::Bash,
            "Task" => ClaudeTool::Task,
            "TodoWrite" => ClaudeTool::TodoWrite,
            "WebFetch" => ClaudeTool::WebFetch,
            other => ClaudeTool::Unknown(other.to_owned()),
        }
    }
}

impl From<ClaudeTool> for Tool {
    fn from(t: ClaudeTool) -> Self {
        match t {
            ClaudeTool::Read => Tool::Read,
            ClaudeTool::Grep => Tool::Grep,
            ClaudeTool::Glob => Tool::Glob,
            ClaudeTool::SemanticSearch => Tool::SemanticSearch,
            ClaudeTool::Write => Tool::Write,
            ClaudeTool::StrReplace => Tool::StrReplace,
            ClaudeTool::Delete => Tool::Delete,
            ClaudeTool::Edit => Tool::Edit,
            ClaudeTool::MultiEdit => Tool::MultiEdit,
            ClaudeTool::EditNotebook => Tool::EditNotebook,
            ClaudeTool::NotebookEdit => Tool::NotebookEdit,
            ClaudeTool::Bash => Tool::Bash,
            ClaudeTool::Task => Tool::Task,
            ClaudeTool::TodoWrite => Tool::TodoWrite,
            ClaudeTool::WebFetch => Tool::WebFetch,
            ClaudeTool::Unknown(s) => Tool::Unknown(s),
        }
    }
}

impl Serialize for ClaudeTool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire_str())
    }
}

impl<'de> Deserialize<'de> for ClaudeTool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(ClaudeTool::from_wire(&s))
    }
}

impl fmt::Display for ClaudeTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

impl JsonSchema for ClaudeTool {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ClaudeTool".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Tool name as sent by Claude Code (PascalCase). Known: Read, Grep, Glob, SemanticSearch, Write, StrReplace, Delete, Edit, MultiEdit, EditNotebook, NotebookEdit, Bash, Task, TodoWrite, WebFetch."
        })
    }
}

// ---------------------------------------------------------------------------
// Cursor tools
// ---------------------------------------------------------------------------

/// Tool names as sent by Cursor (same PascalCase format as Claude Code in practice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorTool {
    Read,
    Grep,
    Glob,
    SemanticSearch,
    Write,
    StrReplace,
    Delete,
    Edit,
    MultiEdit,
    EditNotebook,
    NotebookEdit,
    Bash,
    Task,
    TodoWrite,
    WebFetch,
    Unknown(String),
}

impl CursorTool {
    fn as_wire_str(&self) -> &str {
        match self {
            CursorTool::Read => "Read",
            CursorTool::Grep => "Grep",
            CursorTool::Glob => "Glob",
            CursorTool::SemanticSearch => "SemanticSearch",
            CursorTool::Write => "Write",
            CursorTool::StrReplace => "StrReplace",
            CursorTool::Delete => "Delete",
            CursorTool::Edit => "Edit",
            CursorTool::MultiEdit => "MultiEdit",
            CursorTool::EditNotebook => "EditNotebook",
            CursorTool::NotebookEdit => "NotebookEdit",
            CursorTool::Bash => "Bash",
            CursorTool::Task => "Task",
            CursorTool::TodoWrite => "TodoWrite",
            CursorTool::WebFetch => "WebFetch",
            CursorTool::Unknown(s) => s.as_str(),
        }
    }

    fn from_wire(s: &str) -> CursorTool {
        match s {
            "Read" => CursorTool::Read,
            "Grep" => CursorTool::Grep,
            "Glob" => CursorTool::Glob,
            "SemanticSearch" => CursorTool::SemanticSearch,
            "Write" => CursorTool::Write,
            "StrReplace" => CursorTool::StrReplace,
            "Delete" => CursorTool::Delete,
            "Edit" => CursorTool::Edit,
            "MultiEdit" => CursorTool::MultiEdit,
            "EditNotebook" => CursorTool::EditNotebook,
            "NotebookEdit" => CursorTool::NotebookEdit,
            "Bash" => CursorTool::Bash,
            "Task" => CursorTool::Task,
            "TodoWrite" => CursorTool::TodoWrite,
            "WebFetch" => CursorTool::WebFetch,
            other => CursorTool::Unknown(other.to_owned()),
        }
    }
}

impl From<CursorTool> for Tool {
    fn from(t: CursorTool) -> Self {
        match t {
            CursorTool::Read => Tool::Read,
            CursorTool::Grep => Tool::Grep,
            CursorTool::Glob => Tool::Glob,
            CursorTool::SemanticSearch => Tool::SemanticSearch,
            CursorTool::Write => Tool::Write,
            CursorTool::StrReplace => Tool::StrReplace,
            CursorTool::Delete => Tool::Delete,
            CursorTool::Edit => Tool::Edit,
            CursorTool::MultiEdit => Tool::MultiEdit,
            CursorTool::EditNotebook => Tool::EditNotebook,
            CursorTool::NotebookEdit => Tool::NotebookEdit,
            CursorTool::Bash => Tool::Bash,
            CursorTool::Task => Tool::Task,
            CursorTool::TodoWrite => Tool::TodoWrite,
            CursorTool::WebFetch => Tool::WebFetch,
            CursorTool::Unknown(s) => Tool::Unknown(s),
        }
    }
}

impl Serialize for CursorTool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire_str())
    }
}

impl<'de> Deserialize<'de> for CursorTool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(CursorTool::from_wire(&s))
    }
}

impl fmt::Display for CursorTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire_str())
    }
}

impl JsonSchema for CursorTool {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CursorTool".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Tool name as sent by Cursor (PascalCase). Known: Read, Grep, Glob, SemanticSearch, Write, StrReplace, Delete, Edit, MultiEdit, EditNotebook, NotebookEdit, Bash, Task, TodoWrite, WebFetch."
        })
    }
}

// ===========================================================================
// Provider input/output wire types
// ===========================================================================

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

/// Input from the opencode hook (stdin JSON for both tool.execute.before and permission.ask).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenCodeHookInput {
    pub session_id: String,
    #[serde(rename = "tool_name")]
    pub tool: OpenCodeTool,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub workspace_roots: Option<Vec<String>>,
    #[serde(default = "default_opencode_hook_event")]
    pub hook_event_name: String,
    /// Session title from opencode (may be absent).
    pub session_title: Option<String>,
}

fn default_cwd() -> String {
    ".".to_string()
}

fn default_opencode_hook_event() -> String {
    "tool.execute.before".to_string()
}

/// Output to opencode (stdout JSON).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenCodeHookOutput {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/// Input from Claude Code hook (stdin JSON).
///
/// Claude Code sends tool_name, tool_input, cwd, session_id, hook_event_name.
/// No workspace_roots field — cwd is used as the single root.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudeCodeHookInput {
    pub session_id: String,
    #[serde(rename = "tool_name")]
    pub tool: ClaudeTool,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_claude_hook_event")]
    pub hook_event_name: String,
}

fn default_claude_hook_event() -> String {
    "PreToolUse".to_string()
}

/// Output for Claude Code PreToolUse hook events (stdout JSON).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudePreToolUseOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: ClaudePreToolUseDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudePreToolUseDecision {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "permissionDecision")]
    pub permission_decision: PermissionDecision,
    #[serde(rename = "permissionDecisionReason")]
    pub permission_decision_reason: String,
}

/// Output for Claude Code PermissionRequest hook events (stdout JSON).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudePermissionRequestOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: ClaudePermissionRequestDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudePermissionRequestDecision {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    pub decision: ClaudePermissionBehavior,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClaudePermissionBehavior {
    pub behavior: PermissionDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Input from Cursor hook (stdin JSON).
///
/// Cursor uses `conversation_id` instead of `session_id`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CursorHookInput {
    /// Cursor sends conversation_id; session_id is accepted as fallback.
    pub conversation_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(rename = "tool_name")]
    pub tool: CursorTool,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub workspace_roots: Option<Vec<String>>,
    #[serde(default = "default_cursor_hook_event")]
    pub hook_event_name: String,
}

fn default_cursor_hook_event() -> String {
    "preToolUse".to_string()
}

/// Output to Cursor (stdout JSON).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CursorHookOutput {
    pub permission: PermissionDecision,
    pub user_message: String,
    pub agent_message: String,
}

// ---------------------------------------------------------------------------
// Delegate subprocess types
// ---------------------------------------------------------------------------

/// Canonical payload sent to delegate subprocesses (e.g. Dippy).
///
/// Always serialised as Claude Code's wire format so delegates only need to
/// understand one format regardless of which provider the hook came from.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegatePayload {
    #[serde(rename = "tool_name")]
    pub tool: Tool,
    pub tool_input: serde_json::Value,
    pub cwd: String,
    pub hook_event_name: String,
}

/// Permission decision returned by a delegate subprocess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DelegatePermission {
    Allow,
    Deny,
    Ask,
}

/// Parsed output from a delegate subprocess's stdout.
///
/// The delegate always returns in Claude Code's wire format — the decision
/// lives inside `hookSpecificOutput`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegateOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: DelegateOutputDecision,
}

/// The decision fields inside a delegate's `hookSpecificOutput`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegateOutputDecision {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: DelegatePermission,
    #[serde(rename = "permissionDecisionReason")]
    pub permission_decision_reason: Option<String>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- OpenCodeTool serde ---

    #[test]
    fn opencode_tool_lowercase_deserialize() {
        let json = r#""bash""#;
        let tool: OpenCodeTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool, OpenCodeTool::Bash);
    }

    #[test]
    fn opencode_tool_pascalcase_deserialize() {
        // OpenCode sends PascalCase for tool.execute.before events
        let json = r#""Write""#;
        let tool: OpenCodeTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool, OpenCodeTool::Write);
    }

    #[test]
    fn opencode_tool_unknown_deserialize() {
        let json = r#""my_mcp_tool""#;
        let tool: OpenCodeTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool, OpenCodeTool::Unknown("my_mcp_tool".to_owned()));
    }

    #[test]
    fn opencode_edit_maps_to_write() {
        let tool: Tool = OpenCodeTool::Edit.into();
        assert_eq!(tool, Tool::Write);
    }

    #[test]
    fn opencode_tool_roundtrip() {
        let tool = OpenCodeTool::Bash;
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, r#""bash""#);
        let back: OpenCodeTool = serde_json::from_str(&json).unwrap();
        assert_eq!(back, OpenCodeTool::Bash);
    }

    // --- ClaudeTool serde ---

    #[test]
    fn claude_tool_deserialize() {
        let json = r#""SemanticSearch""#;
        let tool: ClaudeTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool, ClaudeTool::SemanticSearch);
    }

    #[test]
    fn claude_tool_1to1_conversion() {
        let tool: Tool = ClaudeTool::Write.into();
        assert_eq!(tool, Tool::Write);
    }

    // --- CursorTool serde ---

    #[test]
    fn cursor_tool_deserialize() {
        let json = r#""Bash""#;
        let tool: CursorTool = serde_json::from_str(json).unwrap();
        assert_eq!(tool, CursorTool::Bash);
    }

    // --- Input struct wire compat ---

    #[test]
    fn opencode_input_deserializes_tool_name_field() {
        let json = r#"{"session_id":"s1","tool_name":"bash","tool_input":{}}"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.tool, OpenCodeTool::Bash);
    }

    #[test]
    fn claude_input_deserializes_tool_name_field() {
        let json = r#"{"session_id":"s1","tool_name":"Write","tool_input":{}}"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.tool, ClaudeTool::Write);
    }

    #[test]
    fn cursor_input_deserializes_tool_name_field() {
        let json = r#"{"conversation_id":"c1","tool_name":"Read","tool_input":{}}"#;
        let input: CursorHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.tool, CursorTool::Read);
    }

    #[test]
    fn delegate_payload_serializes_tool_name_field() {
        let payload = DelegatePayload {
            tool: Tool::Bash,
            tool_input: serde_json::json!({"command": "ls"}),
            cwd: "/tmp".to_string(),
            hook_event_name: "PreToolUse".to_string(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""tool_name":"Bash""#));
    }

    // --- From conversions ---

    #[test]
    fn all_opencode_known_map_to_known_tool() {
        let mappings = [
            (OpenCodeTool::Bash, Tool::Bash),
            (OpenCodeTool::Edit, Tool::Write),
            (OpenCodeTool::Glob, Tool::Glob),
            (OpenCodeTool::Grep, Tool::Grep),
            (OpenCodeTool::MultiEdit, Tool::MultiEdit),
            (OpenCodeTool::Read, Tool::Read),
            (OpenCodeTool::Task, Tool::Task),
            (OpenCodeTool::TodoWrite, Tool::TodoWrite),
            (OpenCodeTool::WebFetch, Tool::WebFetch),
            (OpenCodeTool::Write, Tool::Write),
        ];
        for (oc, expected) in mappings {
            let got: Tool = oc.into();
            assert_eq!(got, expected);
        }
    }
}

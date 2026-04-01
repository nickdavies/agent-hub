use capabilities::{
    DecisionStatus, HookDecision, ParseError, Provider, ProviderCapabilities, ToolHookEvent,
};
use config::normalise_tool_name;

use super::claude_code::build_display_name;

/// Tool name map for Opencode.
/// Maps opencode permission names (from permission.ask hook) to canonical gateway tool names.
/// Opencode's native tool names for tool.execute.before match Claude Code and pass through unchanged.
const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("bash", "Bash"),
    ("edit", "Write"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("multiedit", "MultiEdit"),
    ("read", "Read"),
    ("task", "Task"),
    ("todowrite", "TodoWrite"),
    ("webfetch", "WebFetch"),
    ("write", "Write"),
];

pub struct Opencode;

impl Provider for Opencode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn parse_input(&self, raw: &str) -> Result<ToolHookEvent, ParseError> {
        let v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| ParseError(format!("invalid JSON: {e}")))?;

        let session_id = v
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ParseError("missing session_id".to_string()))?
            .to_string();

        let tool_name_raw = v
            .get("tool_name")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ParseError("missing tool_name".to_string()))?;

        let tool_name = normalise_tool_name(tool_name_raw, TOOL_NAME_MAP).to_string();

        let tool_input = v
            .get("tool_input")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let cwd = v
            .get("cwd")
            .and_then(|s| s.as_str())
            .unwrap_or(".")
            .to_string();

        let workspace_roots: Vec<String> = v
            .get("workspace_roots")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_else(|| vec![cwd.clone()]);

        let hook_event_name = v
            .get("hook_event_name")
            .and_then(|s| s.as_str())
            .unwrap_or("tool.execute.before")
            .to_string();

        let session_display_name = v
            .get("session_title")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| build_display_name(&session_id, &workspace_roots));

        Ok(ToolHookEvent {
            session_id,
            session_display_name,
            tool_name,
            tool_input,
            cwd,
            workspace_roots,
            hook_event_name,
        })
    }

    fn format_output(&self, _event: &ToolHookEvent, decision: HookDecision) -> String {
        // Opencode currently has inline_approval = false due to the tool.execute.before
        // race condition. Once the upstream fix lands this will need to return a blocking
        // decision. For now we return the decision anyway so it's wired up and ready.
        let allowed = matches!(decision.status, DecisionStatus::Approved);
        let reason = match &decision.status {
            DecisionStatus::DeniedWithReason(r) => Some(r.clone()),
            DecisionStatus::Denied => Some("denied by policy".to_string()),
            DecisionStatus::Approved => None,
        };
        let mut obj = serde_json::json!({"allowed": allowed});
        if let Some(r) = reason {
            obj["reason"] = serde_json::Value::String(r);
        }
        obj.to_string()
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: false,
            agent_ui_prompt: true,
            plan_questions: false,
            rich_context: false,
        }
    }
}

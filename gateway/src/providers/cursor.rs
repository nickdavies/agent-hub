use capabilities::{
    DecisionStatus, HookDecision, ParseError, Provider, ProviderCapabilities, ToolHookEvent,
};
use config::normalise_tool_name;

use super::claude_code::build_display_name;

/// Tool name map for Cursor. Cursor uses Claude Code's tool names natively.
const TOOL_NAME_MAP: &[(&str, &str)] = &[];

pub struct Cursor;

impl Provider for Cursor {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn parse_input(&self, raw: &str) -> Result<ToolHookEvent, ParseError> {
        let v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| ParseError(format!("invalid JSON: {e}")))?;

        // Cursor uses conversation_id rather than session_id
        let session_id = v
            .get("conversation_id")
            .or_else(|| v.get("session_id"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| ParseError("missing conversation_id".to_string()))?
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

        // Cursor sends workspace_roots as an array
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

        // Cursor sends camelCase hook event names
        let hook_event_name = v
            .get("hook_event_name")
            .and_then(|s| s.as_str())
            .unwrap_or("preToolUse")
            .to_string();

        let session_display_name = build_display_name(&session_id, &workspace_roots);

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
        let perm = match &decision.status {
            DecisionStatus::Approved => "allow",
            DecisionStatus::Denied | DecisionStatus::DeniedWithReason(_) => "deny",
        };
        let msg = match &decision.status {
            DecisionStatus::DeniedWithReason(r) => r.clone(),
            _ => decision
                .message
                .clone()
                .unwrap_or_else(|| "resolved via remote approval".to_string()),
        };
        serde_json::json!({
            "permission": perm,
            "user_message": msg,
            "agent_message": msg,
        })
        .to_string()
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: true,
            agent_ui_prompt: false,
            plan_questions: false,
            rich_context: false,
        }
    }
}

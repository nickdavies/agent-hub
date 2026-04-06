use crate::types::{build_display_name, DecisionStatus, HookOutput, ParseError, ToolHookEvent};
use protocol::{CursorHookInput, CursorHookOutput, PermissionDecision, Tool, ToolCall};

impl TryFrom<CursorHookInput> for ToolHookEvent {
    type Error = ParseError;

    fn try_from(input: CursorHookInput) -> Result<Self, Self::Error> {
        let tool: Tool = input.tool.into();
        let tool_call =
            ToolCall::try_from((tool, input.tool_input)).map_err(|e| ParseError(e.to_string()))?;

        // Cursor uses conversation_id rather than session_id
        let session_id = input
            .conversation_id
            .or(input.session_id)
            .ok_or_else(|| ParseError("missing conversation_id".to_string()))?;

        // Cursor sends workspace_roots as an array; fall back to cwd
        let workspace_roots = input
            .workspace_roots
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| vec![input.cwd.clone()]);

        let session_display_name = build_display_name(&session_id, &workspace_roots);

        Ok(ToolHookEvent {
            session_id,
            session_display_name,
            tool_call,
            cwd: input.cwd,
            workspace_roots,
            hook_event_name: input.hook_event_name,
        })
    }
}

/// Format a HookOutput into Cursor's wire format (stdout JSON).
pub fn format_output(_event: &ToolHookEvent, decision: &HookOutput) -> String {
    let perm = match &decision.status {
        DecisionStatus::Approved => PermissionDecision::Allow,
        DecisionStatus::Denied | DecisionStatus::DeniedWithReason(_) => PermissionDecision::Deny,
    };
    let msg = match &decision.status {
        DecisionStatus::DeniedWithReason(r) => r.clone(),
        _ => decision
            .message
            .clone()
            .unwrap_or_else(|| "resolved via remote approval".to_string()),
    };
    let output = CursorHookOutput {
        permission: perm,
        user_message: msg.clone(),
        agent_message: msg,
    };
    serde_json::to_string(&output).expect("CursorHookOutput is always serializable")
}

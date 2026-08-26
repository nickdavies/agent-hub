use crate::types::{
    APPROVAL_PIPELINE_ERROR_MESSAGE, APPROVAL_TIMEOUT_MESSAGE, DecisionStatus, HookOutput,
    ParseError, ToolHookEvent, build_display_name,
};
use protocol::{CursorHookInput, CursorHookOutput, PermissionDecision, Tool, ToolCall};

impl TryFrom<CursorHookInput> for ToolHookEvent {
    type Error = ParseError;

    fn try_from(input: CursorHookInput) -> Result<Self, Self::Error> {
        let tool: Tool = input.tool.into();
        let tool_call =
            ToolCall::try_from((tool, input.tool_input)).map_err(|e| ParseError(e.to_string()))?;

        let session_id = input.session_key.into_session_id();

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
        DecisionStatus::Denied
        | DecisionStatus::DeniedWithReason(_)
        | DecisionStatus::TimedOut
        | DecisionStatus::PipelineError => PermissionDecision::Deny,
    };
    let msg = match &decision.status {
        DecisionStatus::DeniedWithReason(r) => r.clone(),
        DecisionStatus::TimedOut => APPROVAL_TIMEOUT_MESSAGE.to_string(),
        DecisionStatus::PipelineError => APPROVAL_PIPELINE_ERROR_MESSAGE.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event() -> ToolHookEvent {
        ToolHookEvent {
            session_id: protocol::SessionId::new("test-session"),
            session_display_name: "test".to_string(),
            tool_call: ToolCall::try_from((Tool::Bash, serde_json::json!({"command": "ls"})))
                .unwrap(),
            cwd: "/tmp".to_string(),
            workspace_roots: vec!["/tmp".to_string()],
            hook_event_name: "permission.ask".to_string(),
        }
    }

    #[test]
    fn cursor_timeout_and_pipeline_error_deny_with_exact_messages() {
        for (status, message) in [
            (
                DecisionStatus::TimedOut,
                crate::types::APPROVAL_TIMEOUT_MESSAGE,
            ),
            (
                DecisionStatus::PipelineError,
                crate::types::APPROVAL_PIPELINE_ERROR_MESSAGE,
            ),
        ] {
            let output = HookOutput {
                status,
                message: None,
            };
            let value: serde_json::Value =
                serde_json::from_str(&format_output(&test_event(), &output)).unwrap();
            assert_eq!(value["permission"], "deny");
            assert_eq!(value["user_message"], message);
            assert_eq!(value["agent_message"], message);
        }
    }

    #[test]
    fn cursor_explicit_denial_preserves_operator_reason() {
        let output = HookOutput {
            status: DecisionStatus::DeniedWithReason("operator denied this action".to_string()),
            message: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&format_output(&test_event(), &output)).unwrap();
        assert_eq!(value["permission"], "deny");
        assert_eq!(value["user_message"], "operator denied this action");
        assert_eq!(value["agent_message"], "operator denied this action");
    }
}

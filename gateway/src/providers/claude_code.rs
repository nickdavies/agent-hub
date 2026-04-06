use crate::types::{build_display_name, DecisionStatus, HookOutput, ParseError, ToolHookEvent};
use protocol::{
    ClaudeCodeHookInput, ClaudePermissionBehavior, ClaudePermissionRequestDecision,
    ClaudePermissionRequestOutput, ClaudePreToolUseDecision, ClaudePreToolUseOutput,
    PermissionDecision, Tool, ToolCall,
};

impl TryFrom<ClaudeCodeHookInput> for ToolHookEvent {
    type Error = ParseError;

    fn try_from(input: ClaudeCodeHookInput) -> Result<Self, Self::Error> {
        let tool: Tool = input.tool.into();
        let tool_call =
            ToolCall::try_from((tool, input.tool_input)).map_err(|e| ParseError(e.to_string()))?;
        // Claude Code sends `cwd` only; no workspace_roots in the payload.
        // Use cwd as the single workspace root so in_workspace rules work.
        let workspace_roots = vec![input.cwd.clone()];
        let session_display_name =
            build_display_name(&input.session_id, std::slice::from_ref(&input.cwd));

        Ok(ToolHookEvent {
            session_id: input.session_id,
            session_display_name,
            tool_call,
            cwd: input.cwd,
            workspace_roots,
            hook_event_name: input.hook_event_name,
        })
    }
}

/// Format a HookOutput into Claude Code's wire format (stdout JSON).
pub fn format_output(event: &ToolHookEvent, decision: &HookOutput) -> String {
    match event.hook_event_name.to_ascii_lowercase().as_str() {
        "permissionrequest" => {
            let (behavior, message) = match &decision.status {
                DecisionStatus::Approved => (PermissionDecision::Allow, None),
                DecisionStatus::Denied | DecisionStatus::DeniedWithReason(_) => {
                    (PermissionDecision::Deny, Some(deny_message(decision)))
                }
            };
            let output = ClaudePermissionRequestOutput {
                hook_specific_output: ClaudePermissionRequestDecision {
                    hook_event_name: "PermissionRequest".to_string(),
                    decision: ClaudePermissionBehavior { behavior, message },
                },
            };
            serde_json::to_string(&output)
                .expect("ClaudePermissionRequestOutput is always serializable")
        }
        // Default: PreToolUse
        _ => {
            let (perm, reason) = match &decision.status {
                DecisionStatus::Approved => (
                    PermissionDecision::Allow,
                    decision.message.clone().unwrap_or_default(),
                ),
                DecisionStatus::Denied => {
                    (PermissionDecision::Deny, "denied by policy".to_string())
                }
                DecisionStatus::DeniedWithReason(r) => (PermissionDecision::Deny, r.clone()),
            };
            let output = ClaudePreToolUseOutput {
                hook_specific_output: ClaudePreToolUseDecision {
                    hook_event_name: "PreToolUse".to_string(),
                    permission_decision: perm,
                    permission_decision_reason: reason,
                },
            };
            serde_json::to_string(&output).expect("ClaudePreToolUseOutput is always serializable")
        }
    }
}

fn deny_message(decision: &HookOutput) -> String {
    match &decision.status {
        DecisionStatus::DeniedWithReason(r) => r.clone(),
        _ => decision
            .message
            .clone()
            .unwrap_or_else(|| "denied via remote approval".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- format_output: case-insensitive event matching ---

    fn test_event(hook_event_name: &str) -> ToolHookEvent {
        let tool_call =
            ToolCall::try_from((protocol::Tool::Bash, serde_json::json!({"command": "ls"})))
                .unwrap();
        ToolHookEvent {
            session_id: "test-session".to_string(),
            session_display_name: "test".to_string(),
            tool_call,
            cwd: "/tmp".to_string(),
            workspace_roots: vec!["/tmp".to_string()],
            hook_event_name: hook_event_name.to_string(),
        }
    }

    fn approved() -> HookOutput {
        HookOutput {
            status: DecisionStatus::Approved,
            message: None,
        }
    }

    fn denied() -> HookOutput {
        HookOutput {
            status: DecisionStatus::Denied,
            message: None,
        }
    }

    #[test]
    fn claude_output_pretooluse_pascal_case() {
        let out = format_output(&test_event("PreToolUse"), &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn claude_output_pretooluse_camel_case() {
        let out = format_output(&test_event("preToolUse"), &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn claude_output_permissionrequest_pascal_case() {
        let out = format_output(&test_event("PermissionRequest"), &denied());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[test]
    fn claude_output_permissionrequest_camel_case() {
        let out = format_output(&test_event("permissionRequest"), &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "allow");
    }

    #[test]
    fn claude_output_unsupported_event_falls_to_pretooluse() {
        // In the new API, unknown events fall through to the PreToolUse branch
        // (no error return — this is a deliberate design difference from the old code).
        let out = format_output(&test_event("PostToolUse"), &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    }
}

use crate::types::{
    APPROVAL_TIMEOUT_MESSAGE, DecisionStatus, HookOutput, ParseError, ToolHookEvent,
    approval_pipeline_error_message, bounded_gateway_message, build_display_name,
};
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
    format_for_event(&event.hook_event_name, decision)
}

fn format_for_event(hook_event_name: &str, decision: &HookOutput) -> String {
    match hook_event_name.to_ascii_lowercase().as_str() {
        "permissionrequest" => {
            let (behavior, message) = match &decision.status {
                DecisionStatus::Approved => (PermissionDecision::Allow, None),
                DecisionStatus::Denied
                | DecisionStatus::DeniedWithReason(_)
                | DecisionStatus::TimedOut
                | DecisionStatus::PipelineError(_) => {
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
                DecisionStatus::DeniedWithReason(r) => {
                    (PermissionDecision::Deny, bounded_gateway_message(r))
                }
                DecisionStatus::TimedOut => (
                    PermissionDecision::Deny,
                    APPROVAL_TIMEOUT_MESSAGE.to_string(),
                ),
                DecisionStatus::PipelineError(detail) => (
                    PermissionDecision::Deny,
                    approval_pipeline_error_message(detail),
                ),
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

pub fn format_blocking_denial(raw: &str, reason: &str) -> Option<String> {
    let hook_event_name = serde_json::from_str::<serde_json::Value>(raw)
        .ok()?
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("PreToolUse")
        .to_string();
    Some(format_for_event(
        &hook_event_name,
        &HookOutput {
            status: DecisionStatus::DeniedWithReason(bounded_gateway_message(reason)),
            message: None,
        },
    ))
}

fn deny_message(decision: &HookOutput) -> String {
    match &decision.status {
        DecisionStatus::Denied => "denied by policy".to_string(),
        DecisionStatus::DeniedWithReason(r) => bounded_gateway_message(r),
        DecisionStatus::TimedOut => APPROVAL_TIMEOUT_MESSAGE.to_string(),
        DecisionStatus::PipelineError(detail) => approval_pipeline_error_message(detail),
        DecisionStatus::Approved => decision
            .message
            .clone()
            .unwrap_or_else(|| "denied via remote approval".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SessionId;

    // --- format_output: case-insensitive event matching ---

    fn test_event(hook_event_name: &str) -> ToolHookEvent {
        let tool_call =
            ToolCall::try_from((protocol::Tool::Bash, serde_json::json!({"command": "ls"})))
                .unwrap();
        ToolHookEvent {
            session_id: SessionId::new("test-session"),
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

    fn outcome(status: DecisionStatus) -> HookOutput {
        HookOutput {
            status,
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

    #[test]
    fn claude_timeout_and_pipeline_error_deny_with_exact_messages() {
        for (status, message) in [
            (
                DecisionStatus::TimedOut,
                crate::types::APPROVAL_TIMEOUT_MESSAGE,
            ),
            (
                DecisionStatus::PipelineError("test failure".into()),
                "Agent Hub denied tool execution because the approval pipeline failed (gateway): test failure",
            ),
        ] {
            let output = outcome(status);
            let out = format_output(&test_event("PreToolUse"), &output);
            let value: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
            assert_eq!(
                value["hookSpecificOutput"]["permissionDecisionReason"],
                message
            );

            let out = format_output(&test_event("PermissionRequest"), &output);
            let value: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(value["hookSpecificOutput"]["decision"]["behavior"], "deny");
            assert_eq!(value["hookSpecificOutput"]["decision"]["message"], message);
        }
    }

    #[test]
    fn claude_explicit_denial_preserves_operator_reason() {
        let output = outcome(DecisionStatus::DeniedWithReason(
            "operator denied this action".to_string(),
        ));
        for event_name in ["PreToolUse", "PermissionRequest"] {
            let value: serde_json::Value =
                serde_json::from_str(&format_output(&test_event(event_name), &output)).unwrap();
            let reason = if event_name == "PreToolUse" {
                &value["hookSpecificOutput"]["permissionDecisionReason"]
            } else {
                &value["hookSpecificOutput"]["decision"]["message"]
            };
            assert_eq!(reason, "operator denied this action");
        }
    }
}

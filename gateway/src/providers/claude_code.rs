use capabilities::{
    DecisionStatus, HookDecision, ParseError, Provider, ProviderCapabilities, ToolHookEvent,
};
use config::normalise_tool_name;

/// Tool name map for Claude Code. Claude Code uses its own canonical names natively.
const TOOL_NAME_MAP: &[(&str, &str)] = &[];

pub struct ClaudeCode;

impl Provider for ClaudeCode {
    fn name(&self) -> &'static str {
        "claude-code"
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

        // Claude Code sends `cwd` only; no workspace_roots in the payload.
        // Use cwd as the single workspace root so in_workspace rules work.
        let workspace_roots = vec![cwd.clone()];

        let hook_event_name = v
            .get("hook_event_name")
            .and_then(|s| s.as_str())
            .unwrap_or("PreToolUse")
            .to_string();

        let session_display_name = build_display_name(&session_id, std::slice::from_ref(&cwd));

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

    fn format_output(&self, event: &ToolHookEvent, decision: HookDecision) -> String {
        format_claude_output(&event.hook_event_name, &decision)
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: true,
            agent_ui_prompt: false,
            plan_questions: false,
            rich_context: true,
        }
    }
}

fn format_claude_output(hook_event: &str, decision: &HookDecision) -> String {
    match hook_event.to_ascii_lowercase().as_str() {
        "permissionrequest" => {
            let inner = match &decision.status {
                DecisionStatus::Approved => serde_json::json!({"behavior": "allow"}),
                DecisionStatus::Denied | DecisionStatus::DeniedWithReason(_) => {
                    let msg = deny_message(decision);
                    serde_json::json!({"behavior": "deny", "message": msg})
                }
            };
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": inner
                }
            })
            .to_string()
        }
        // Default: PreToolUse
        _ => {
            let (perm, reason) = match &decision.status {
                DecisionStatus::Approved => ("allow", decision.message.as_deref().unwrap_or("")),
                DecisionStatus::Denied => ("deny", "denied by policy"),
                DecisionStatus::DeniedWithReason(r) => ("deny", r.as_str()),
            };
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": perm,
                    "permissionDecisionReason": reason
                }
            })
            .to_string()
        }
    }
}

fn deny_message(decision: &HookDecision) -> String {
    match &decision.status {
        DecisionStatus::DeniedWithReason(r) => r.clone(),
        _ => decision
            .message
            .clone()
            .unwrap_or_else(|| "denied via remote approval".to_string()),
    }
}

/// Build a human-readable display name from session ID and workspace roots.
pub fn build_display_name(session_id: &str, roots: &[String]) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let normalize = |p: &str| -> String {
        if !home.is_empty() && p.starts_with(&home) {
            format!("~{}", &p[home.len()..])
        } else {
            p.to_string()
        }
    };

    let id_part = &session_id[..session_id.len().min(8)];

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

#[cfg(test)]
mod tests {
    use super::*;

    // --- format_claude_output: case-insensitive event matching ---

    fn approved() -> HookDecision {
        HookDecision {
            status: DecisionStatus::Approved,
            message: None,
        }
    }

    fn denied() -> HookDecision {
        HookDecision {
            status: DecisionStatus::Denied,
            message: None,
        }
    }

    #[test]
    fn claude_output_pretooluse_pascal_case() {
        let out = format_claude_output("PreToolUse", &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn claude_output_pretooluse_camel_case() {
        let out = format_claude_output("preToolUse", &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }

    #[test]
    fn claude_output_permissionrequest_pascal_case() {
        let out = format_claude_output("PermissionRequest", &denied());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[test]
    fn claude_output_permissionrequest_camel_case() {
        let out = format_claude_output("permissionRequest", &approved());
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
        let out = format_claude_output("PostToolUse", &approved());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    }
}

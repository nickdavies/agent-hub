use crate::types::{
    APPROVAL_PIPELINE_ERROR_MESSAGE, APPROVAL_TIMEOUT_MESSAGE, DecisionStatus, HookOutput,
    ParseError, ToolHookEvent, build_display_name,
};
use config::normalize_path;
use protocol::{OpenCodeHookInput, OpenCodeHookOutput, Tool, ToolCall};

impl TryFrom<OpenCodeHookInput> for ToolHookEvent {
    type Error = ParseError;

    fn try_from(input: OpenCodeHookInput) -> Result<Self, Self::Error> {
        if input.cwd.is_empty() {
            return Err(ParseError("OpenCode cwd must not be empty".to_string()));
        }
        let cwd = std::path::Path::new(&input.cwd);
        if !cwd.is_absolute() {
            return Err(ParseError("OpenCode cwd must be absolute".to_string()));
        }
        let cwd = normalize_path(cwd).to_string_lossy().into_owned();

        let tool: Tool = input.tool.into();
        let tool_call =
            ToolCall::try_from((tool, input.tool_input)).map_err(|e| ParseError(e.to_string()))?;

        // OpenCode sends workspace_roots; fall back to cwd
        let workspace_roots = input
            .workspace_roots
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| vec![cwd.clone()]);

        // Prefer session_title if available, else build from session_id + roots
        let session_display_name = input
            .session_title
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| build_display_name(&input.session_id, &workspace_roots));

        Ok(ToolHookEvent {
            session_id: input.session_id,
            session_display_name,
            tool_call,
            cwd,
            workspace_roots,
            hook_event_name: input.hook_event_name,
        })
    }
}

/// Format a HookOutput into Opencode's wire format (stdout JSON).
///
/// Opencode currently has inline_approval = false due to the tool.execute.before
/// race condition. Once the upstream fix lands this will need to return a blocking
/// decision. For now we return the decision anyway so it's wired up and ready.
pub fn format_output(_event: &ToolHookEvent, decision: &HookOutput) -> String {
    let allowed = matches!(decision.status, DecisionStatus::Approved);
    let reason = match &decision.status {
        DecisionStatus::DeniedWithReason(r) => Some(r.clone()),
        DecisionStatus::Denied => Some("denied by policy".to_string()),
        DecisionStatus::TimedOut => Some(APPROVAL_TIMEOUT_MESSAGE.to_string()),
        DecisionStatus::PipelineError => Some(APPROVAL_PIPELINE_ERROR_MESSAGE.to_string()),
        DecisionStatus::Approved => None,
    };
    let output = OpenCodeHookOutput { allowed, reason };
    serde_json::to_string(&output).expect("OpenCodeHookOutput is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{ConfigAction, ConfigDecision, load_tool_config, resolve_action};

    fn parse_input(cwd: &str) -> OpenCodeHookInput {
        serde_json::from_value(serde_json::json!({
            "session_id": "session-1",
            "tool_name": "mcp_weather_lookup",
            "tool_input": {},
            "cwd": cwd
        }))
        .expect("wire input deserializes")
    }

    fn parse_read_input(cwd: &str, path: &str) -> OpenCodeHookInput {
        serde_json::from_value(serde_json::json!({
            "session_id": "session-1",
            "tool_name": "read",
            "tool_input": {"path": path},
            "cwd": cwd
        }))
        .expect("wire input deserializes")
    }

    fn test_event() -> ToolHookEvent {
        ToolHookEvent::try_from(parse_input("/home/user/project")).unwrap()
    }

    #[test]
    fn opencode_timeout_and_pipeline_error_deny_with_exact_messages() {
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
            assert_eq!(value["allowed"], false);
            assert_eq!(value["reason"], message);
        }
    }

    #[test]
    fn opencode_explicit_denial_preserves_operator_reason() {
        let output = HookOutput {
            status: DecisionStatus::DeniedWithReason("operator denied this action".to_string()),
            message: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&format_output(&test_event(), &output)).unwrap();
        assert_eq!(value["allowed"], false);
        assert_eq!(value["reason"], "operator denied this action");
    }

    #[test]
    fn empty_opencode_cwd_parses_but_conversion_rejects_it() {
        let input = parse_input("");

        let error = match ToolHookEvent::try_from(input) {
            Ok(_) => panic!("empty cwd must fail closed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn relative_opencode_cwd_parses_but_conversion_rejects_it() {
        let input = parse_input("relative/project");

        let error = match ToolHookEvent::try_from(input) {
            Ok(_) => panic!("relative cwd must fail closed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn conversion_normalizes_cwd_before_scoped_rule_matching() {
        let path = std::env::temp_dir().join(format!(
            "agent-hub-gateway-in-cwds-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "default": "deny",
                "rules": [{
                    "tools": ["mcp_weather_lookup"],
                    "action": "allow",
                    "in_cwds": ["/home/user/sensitive"]
                }]
            }))
            .expect("config serializes"),
        )
        .expect("config is writable");
        let config = load_tool_config(path.to_str().expect("temporary path is UTF-8"))
            .expect("config loads");
        std::fs::remove_file(path).expect("config is removable");

        let matching_event =
            ToolHookEvent::try_from(parse_input("/home/user/ordinary/../sensitive/./project"))
                .expect("absolute cwd converts");
        assert_eq!(matching_event.cwd, "/home/user/sensitive/project");
        assert!(matches!(
            resolve_action(
                &config,
                &matching_event.tool_call.tool(),
                &[],
                Some(&matching_event.cwd),
                None,
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));

        let collision_event =
            ToolHookEvent::try_from(parse_input("/home/user/ordinary/../sensitive-project"))
                .expect("absolute cwd converts");
        assert!(matches!(
            resolve_action(
                &config,
                &collision_event.tool_call.tool(),
                &[],
                Some(&collision_event.cwd),
                None,
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn conversion_normalizes_fallback_workspace_root_for_in_workspace_rule() {
        let path = std::env::temp_dir().join(format!(
            "agent-hub-gateway-in-workspace-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "default": "deny",
                "rules": [{
                    "tools": ["Read"],
                    "action": "allow",
                    "in_workspace": true
                }]
            }))
            .expect("config serializes"),
        )
        .expect("config is writable");
        let config = load_tool_config(path.to_str().expect("temporary path is UTF-8"))
            .expect("config loads");
        std::fs::remove_file(path).expect("config is removable");

        let event = ToolHookEvent::try_from(parse_read_input(
            "/home/user/project/./crates/..",
            "/home/user/project/src/main.rs",
        ))
        .expect("absolute cwd converts");
        assert_eq!(event.cwd, "/home/user/project");
        assert_eq!(event.workspace_roots, vec!["/home/user/project"]);

        let tool = event.tool_call.tool();
        let resolved_args = event
            .tool_call
            .matchable_args()
            .iter()
            .map(|arg| config::resolve_path(arg, &tool, Some(&event.cwd)))
            .collect::<Vec<_>>();
        assert!(matches!(
            resolve_action(
                &config,
                &tool,
                &resolved_args,
                Some(&event.cwd),
                Some(&event.workspace_roots),
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }
}

use chrono::{DateTime, Utc};
use serde::Serialize;

use protocol::{
    AgentHubRunResolvedV1, Approval, ApprovalContext, ApprovalRequest,
    AttendedAuthorizationRequestV4, RequestType,
};

const AGENT_HUB_RUN_TOOL: &str = "agent_hub_run_resolved";
const AGENT_HUB_RUN_PROVIDER: &str = "agent-hub-host-launch";
const MAX_REVIEW_VALUE_CHARS: usize = 4096;
const MAX_RAW_STRING_CHARS: usize = 64 * 1024;
const MAX_RAW_ARRAY_ITEMS: usize = 256;

#[derive(Debug, Serialize)]
pub struct ApprovalReview {
    pub approval: Approval,
    pub launch_review: Option<LaunchReview>,
    pub raw_record: String,
}

#[derive(Debug, Serialize)]
pub struct LaunchReview {
    pub sections: Vec<LaunchReviewSection>,
}

#[derive(Debug, Serialize)]
pub struct LaunchReviewSection {
    pub id: &'static str,
    pub title: &'static str,
    pub value: String,
}

impl ApprovalReview {
    pub fn new(approval: Approval) -> Result<Self, String> {
        let launch_review = launch_review(&approval)?;
        let raw_record = bounded_approval_json(&approval)?;
        Ok(Self {
            approval,
            launch_review,
            raw_record,
        })
    }
}

pub fn validate_request(request: &ApprovalRequest) -> Result<(), String> {
    if request.tool.as_str() == AGENT_HUB_RUN_TOOL {
        let (resolved, _) = parse_launch(
            &request.tool_input,
            request.request_digest.as_deref(),
            Utc::now(),
        )?;
        let event = resolved.gateway_event()?;
        let expected = ApprovalRequest {
            id: resolved.invocation_id.clone(),
            request_digest: Some(resolved.permission_scope_digest()?),
            session_id: event.session_id,
            session_display_name: event
                .session_title
                .ok_or_else(|| "agent_hub_run canonical gateway event has no title".to_string())?,
            cwd: event.cwd,
            tool: event.tool.into(),
            tool_input: event.tool_input,
            provider: AGENT_HUB_RUN_PROVIDER.to_string(),
            request_type: RequestType::ToolUse,
            context: ApprovalContext {
                workspace_roots: event.workspace_roots.ok_or_else(|| {
                    "agent_hub_run canonical gateway event has no workspace roots".to_string()
                })?,
                hook_event_name: event.hook_event_name.into(),
                extra: None,
                attended: None,
            },
        };
        if request != &expected {
            return Err("agent_hub_run_resolved approval envelope binding mismatch".to_string());
        }
    }
    Ok(())
}

fn launch_review(approval: &Approval) -> Result<Option<LaunchReview>, String> {
    if approval.tool.as_str() != AGENT_HUB_RUN_TOOL {
        return Ok(None);
    }
    let (request, execution) = parse_launch(
        &approval.tool_input,
        approval.request_digest.as_deref(),
        Utc::now(),
    )?;
    let proposal = &execution.proposal;
    let approved = &execution.execution;
    let runtime = &approved.runtime;
    let permission_scope_digest = request.permission_scope_digest()?;

    let sections = vec![
        section(
            "policy",
            "Policy",
            serde_json::json!({
                "tool_approval": proposal.tool_approval,
                "runtime_version": runtime.runtime_version,
                "config_root": approved.config_root,
                "config_provenance": approved.config_provenance,
                "permissions": runtime.permissions,
                "approval_rules": runtime.approval_rules,
                "behavior": approved.behavior,
                "approval_bundle": execution.approval_bundle,
            }),
        )?,
        section(
            "digests",
            "Digests",
            serde_json::json!({
                "approval_request_digest": approval.request_digest,
                "permission_scope_digest": permission_scope_digest,
                "resolved_request_digest": request.request_digest,
                "canonical_digest": request.canonical_digest,
                "canonical_execution_digest": request.canonical_execution.digest,
                "prompt_digest": proposal.prompt_digest,
                "image_id": proposal.image_id,
                "config_digest": approved.config_digest,
                "environment_bundle_digest": approved.environment_bundle_digest,
                "approval_bundle_digest": execution.approval_bundle_digest,
            }),
        )?,
        section(
            "validity",
            "Validity",
            serde_json::json!({
                "requested_at": request.requested_at,
                "expires_at": request.expires_at,
            }),
        )?,
        section("prompt", "Prompt", serde_json::json!(proposal.prompt))?,
        section(
            "workspaces",
            "Workspaces",
            serde_json::json!({
                "primary": approved.primary_workspace,
                "workspaces": approved.workspaces,
                "context_roots": approved.context_roots,
                "resolved_roots": execution.roots,
                "workspace_root_identities": execution.workspace_root_identities,
                "context_root_identities": execution.context_root_identities,
                "repositories": approved.repositories,
                "workspace_export": execution.workspace_export,
            }),
        )?,
        section(
            "git-views",
            "Git views",
            serde_json::json!(execution.git_views),
        )?,
        section(
            "image",
            "Image",
            serde_json::json!({
                "family": proposal.image_family,
                "tag": proposal.image_tag,
                "id": proposal.image_id,
                "runtime": runtime.environment.container_image,
            }),
        )?,
        section(
            "network",
            "Network",
            serde_json::json!({
                "class": proposal.network_class,
                "resources": proposal.network_resources,
                "runtime": runtime.environment.network_resources,
            }),
        )?,
        section(
            "credentials",
            "Credentials",
            serde_json::json!({
                "providers": approved.credential_providers,
                "overlays": approved.overlays,
                "repository_claims": runtime.repos,
                "github_capabilities": runtime.github_capabilities,
            }),
        )?,
        section(
            "limits",
            "Limits",
            serde_json::json!({
                "timeout_secs": proposal.timeout_secs,
                "memory": runtime.environment.memory_limit,
                "cpu": runtime.environment.cpu_limit,
                "pids": approved.pids_limit,
                "retry": execution.retry,
                "output": execution.output.limits,
                "lifecycle": execution.lifecycle,
            }),
        )?,
        section(
            "output-ids",
            "Output IDs",
            serde_json::json!({
                "transcript": execution.output.transcript,
                "result": execution.output.result,
                "artifacts": execution.output.artifacts,
                "repositories": execution.output.repositories,
                "cleanup": execution.output.cleanup,
            }),
        )?,
    ];
    Ok(Some(LaunchReview { sections }))
}

fn section(
    id: &'static str,
    title: &'static str,
    value: serde_json::Value,
) -> Result<LaunchReviewSection, String> {
    Ok(LaunchReviewSection {
        id,
        title,
        value: bounded_pretty_json(&value, MAX_REVIEW_VALUE_CHARS)?,
    })
}

fn parse_launch(
    value: &serde_json::Value,
    approval_request_digest: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(AgentHubRunResolvedV1, AttendedAuthorizationRequestV4), String> {
    let request: AgentHubRunResolvedV1 = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid agent_hub_run_resolved payload: {error}"))?;
    request.validate(now)?;
    let permission_scope_digest = request.permission_scope_digest()?;
    if approval_request_digest != Some(permission_scope_digest.as_str()) {
        return Err("agent_hub_run_resolved digest binding mismatch".to_string());
    }
    let execution = request.canonical_execution.decode()?.verify_v4()?;
    Ok((request, execution))
}

fn bounded_pretty_json<T: Serialize>(value: &T, max_chars: usize) -> Result<String, String> {
    let rendered = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    Ok(truncate_chars(&rendered, max_chars))
}

fn bounded_approval_json(approval: &Approval) -> Result<String, String> {
    let mut value = serde_json::to_value(approval).map_err(|error| error.to_string())?;
    bound_raw_value(&mut value);
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

fn bound_raw_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = truncate_chars(text, MAX_RAW_STRING_CHARS),
        serde_json::Value::Array(items) => {
            let truncated = items.len().saturating_sub(MAX_RAW_ARRAY_ITEMS);
            items.truncate(MAX_RAW_ARRAY_ITEMS);
            for item in items.iter_mut() {
                bound_raw_value(item);
            }
            if truncated > 0 {
                items.push(serde_json::Value::String(format!(
                    "… ({truncated} more items)"
                )));
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                bound_raw_value(value);
            }
        }
        _ => {}
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    match value.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}\n… (truncated)", &value[..end]),
        None => value.to_owned(),
    }
}

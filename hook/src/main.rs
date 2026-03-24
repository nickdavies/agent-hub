use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Claude Code approval hook binary.
///
/// Reads hook payload from stdin, checks session approval mode, and either
/// falls through (terminal mode) or blocks until a remote decision is made.
///
/// Exit codes:
///   0 = success (output on stdout if decision made, no output for fall-through)
///   2 = block (error occurred, fail-closed)
#[derive(Parser)]
#[command(name = "claude-approve", version)]
struct Args {
    /// Server URL (e.g. https://notify.example.com)
    #[arg(long, env = "CLAUDE_NOTIFY_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "CLAUDE_NOTIFY_TOKEN")]
    token: String,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "600")]
    timeout: u64,
}

/// Minimal hook payload from Claude Code (stdin).
#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct ApprovalModeResponse {
    approval_mode: String,
}

#[derive(Serialize)]
struct ApprovalRequest {
    request_id: String,
    session_id: String,
    cwd: String,
    tool_name: String,
    tool_input: serde_json::Value,
}

#[derive(Deserialize)]
struct ApprovalResponse {
    id: Uuid,
    #[serde(rename = "type")]
    status_type: String,
    message: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct WaitResponse {
    #[serde(rename = "type")]
    status_type: String,
    message: Option<String>,
    reason: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claude-approve: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: &Args) -> Result<(), String> {
    // Read hook payload from stdin
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let payload: HookInput =
        serde_json::from_str(&input).map_err(|e| format!("failed to parse hook payload: {e}"))?;

    let session_id = payload
        .session_id
        .as_deref()
        .ok_or("missing session_id in payload")?;
    let hook_event = payload
        .hook_event_name
        .as_deref()
        .ok_or("missing hook_event_name in payload")?;
    let tool_name = payload
        .tool_name
        .as_deref()
        .unwrap_or("unknown");
    let tool_input = payload
        .tool_input
        .clone()
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let cwd = payload.cwd.as_deref().unwrap_or(".");

    let client = reqwest::Client::new();
    let base = args.server.trim_end_matches('/');

    // Check session approval mode
    let mode_url = format!("{base}/api/v1/sessions/{session_id}/approval-mode");
    let mode_resp = client
        .get(&mode_url)
        .bearer_auth(&args.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to check approval mode: {e}"))?;

    if !mode_resp.status().is_success() {
        // Session not found - default to remote for safety
        eprintln!(
            "claude-approve: session {session_id} not found on server ({}), defaulting to remote",
            mode_resp.status()
        );
    } else {
        let mode: ApprovalModeResponse = mode_resp
            .json()
            .await
            .map_err(|e| format!("failed to parse approval mode: {e}"))?;

        if mode.approval_mode == "terminal" {
            // Fall through: exit 0 with no output -> Claude shows normal dialog
            return Ok(());
        }
    }

    // Remote mode: register approval and wait
    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    let register_resp = client
        .post(&register_url)
        .bearer_auth(&args.token)
        .json(&ApprovalRequest {
            request_id: request_id.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
        })
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to register approval: {e}"))?;

    if !register_resp.status().is_success() {
        return Err(format!(
            "server returned {} for approval registration",
            register_resp.status()
        ));
    }

    let approval: ApprovalResponse = register_resp
        .json()
        .await
        .map_err(|e| format!("failed to parse approval response: {e}"))?;

    // If already resolved, output immediately
    if approval.status_type != "pending" {
        let output = format_output(
            hook_event,
            &approval.status_type,
            approval.message.as_deref(),
            approval.reason.as_deref(),
        )?;
        print!("{output}");
        return Ok(());
    }

    // Long-poll loop
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("approval timed out".to_string());
        }

        let resp = client
            .get(&wait_url)
            .bearer_auth(&args.token)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("wait request failed: {e}"))?;

        let status = resp.status();
        let wait: WaitResponse = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse wait response: {e}"))?;

        if status.as_u16() == 202 || wait.status_type == "pending" {
            // Timeout, retry
            continue;
        }

        let output = format_output(
            hook_event,
            &wait.status_type,
            wait.message.as_deref(),
            wait.reason.as_deref(),
        )?;
        print!("{output}");
        return Ok(());
    }
}

/// Format the hook output JSON based on event type and decision.
fn format_output(
    hook_event: &str,
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    match hook_event {
        "PermissionRequest" => {
            let decision = match status_type {
                "approved" => {
                    serde_json::json!({
                        "behavior": "allow"
                    })
                }
                "denied" => {
                    serde_json::json!({
                        "behavior": "deny",
                        "message": reason.or(message).unwrap_or("denied via remote approval")
                    })
                }
                other => return Err(format!("unexpected approval status: {other}")),
            };
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": decision
                }
            });
            serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
        }
        "PreToolUse" => {
            let (perm_decision, perm_reason) = match status_type {
                "approved" => ("allow", message.unwrap_or("")),
                "denied" => ("deny", reason.or(message).unwrap_or("denied via remote approval")),
                other => return Err(format!("unexpected approval status: {other}")),
            };
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": perm_decision,
                    "permissionDecisionReason": perm_reason
                }
            });
            serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
        }
        other => Err(format!("unsupported hook event: {other}")),
    }
}

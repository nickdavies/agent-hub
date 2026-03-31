mod providers;

use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use capabilities::{
    ApprovalContext, DecisionStatus, HookDecision, Provider, ResolvedAction, ToolCategory,
    ToolHookEvent, default_to_resolved, expand_tilde, find_tool_def, load_tool_config,
    resolve_action,
};
use clap::Parser;
use providers::{claude_code::ClaudeCode, cursor::Cursor, opencode::Opencode};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use uuid::Uuid;

/// Agent Hub gateway — provider-agnostic tool approval hook.
///
/// Exit codes:
///   0 = success (approval decision written to stdout)
///   1 = server unreachable (connection error, timeout); agent should ask the user
///   2 = fail-closed (bad input, config error, policy denial); agent should deny
#[derive(Parser)]
#[command(name = "agent-hub-gateway", version)]
struct Cli {
    /// Use Claude Code provider
    #[arg(long, conflicts_with_all = ["cursor", "opencode"])]
    claude: bool,

    /// Use Cursor provider
    #[arg(long, conflicts_with_all = ["claude", "opencode"])]
    cursor: bool,

    /// Use Opencode provider
    #[arg(long, conflicts_with_all = ["claude", "cursor"])]
    opencode: bool,

    /// Server URL (e.g. https://hub.example.com)
    #[arg(long, env = "AGENT_HUB_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "AGENT_HUB_TOKEN")]
    token: String,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "600")]
    timeout: u64,

    /// Path to tool routing config file (JSON)
    #[arg(long, default_value = "~/.config/agent-hub/tools.json")]
    config: String,
}

// --- Server wire types ---

#[derive(Serialize)]
struct ApprovalRequest {
    id: String,
    session_id: String,
    session_display_name: String,
    cwd: String,
    tool_name: String,
    tool_input: serde_json::Value,
    provider: String,
    request_type: &'static str,
    context: ApprovalContext,
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

// --- Delegate subprocess result ---

/// Canonical payload sent to delegate subprocesses (e.g. Dippy).
///
/// Always serialised as Claude Code's wire format so delegates only need to
/// understand one format regardless of which provider the hook came from.
/// The delegate should be invoked with `--claude` (or equivalent).
#[derive(Serialize)]
struct DelegatePayload<'a> {
    tool_name: &'a str,
    tool_input: &'a serde_json::Value,
    cwd: &'a str,
    hook_event_name: &'a str,
}

impl<'a> DelegatePayload<'a> {
    fn from_event(event: &'a ToolHookEvent) -> Self {
        // Delegates always receive Claude Code format. Normalise shell tool names to
        // "Bash" so delegates only need to handle one shell tool name.
        let tool_name = match find_tool_def(&event.tool_name) {
            Some(def) if matches!(def.category, ToolCategory::Shell) => "Bash",
            _ => &event.tool_name,
        };
        Self {
            tool_name,
            tool_input: &event.tool_input,
            cwd: &event.cwd,
            hook_event_name: &event.hook_event_name,
        }
    }
}

struct DelegateResult {
    /// "allow", "deny", "ask", or None if output couldn't be parsed
    permission: Option<String>,
    /// Reason text from permissionDecisionReason (Dippy) or equivalent field.
    reason: Option<String>,
}

// --- Entrypoint ---

enum RunError {
    /// Hard failure — bad input, config error, etc. Agent should deny.
    FailClosed(String),
    /// Server unreachable — connection error, timeout, bad response. Agent should ask the user.
    ServerUnreachable(String),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let provider: Box<dyn Provider> = if cli.claude {
        Box::new(ClaudeCode)
    } else if cli.cursor {
        Box::new(Cursor)
    } else if cli.opencode {
        Box::new(Opencode)
    } else {
        eprintln!("agent-hub-gateway: one of --claude, --cursor, or --opencode is required");
        return ExitCode::from(2);
    };

    let config_path = expand_tilde(&cli.config);

    match run(&cli, &*provider, &config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::ServerUnreachable(e)) => {
            eprintln!("agent-hub-gateway: {e}");
            ExitCode::from(1)
        }
        Err(RunError::FailClosed(e)) => {
            eprintln!("agent-hub-gateway: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: &Cli, provider: &dyn Provider, config_path: &str) -> Result<(), RunError> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| RunError::FailClosed(format!("failed to read stdin: {e}")))?;

    let event = provider
        .parse_input(&input)
        .map_err(|e| RunError::FailClosed(format!("failed to parse hook payload: {e}")))?;

    eprintln!(
        "[info] tool={} session={} cwd={}",
        event.tool_name, event.session_id, event.cwd
    );

    let config = load_tool_config(config_path).map_err(RunError::FailClosed)?;

    let action = resolve_action(
        &config,
        &event.tool_name,
        Some(&event.tool_input),
        Some(&event.cwd),
        Some(&event.workspace_roots),
    );

    eprintln!("[info] resolved action: {:?}", action);

    let decision = match action {
        ResolvedAction::Allow => {
            eprintln!("[info] allowing tool={}", event.tool_name);
            HookDecision {
                status: DecisionStatus::Approved,
                message: None,
            }
        }
        ResolvedAction::Deny(msg) => {
            eprintln!("[info] denying tool={} reason={:?}", event.tool_name, msg);
            HookDecision {
                status: match msg {
                    Some(m) => DecisionStatus::DeniedWithReason(m),
                    None => DecisionStatus::Denied,
                },
                message: None,
            }
        }
        ResolvedAction::Ask => {
            eprintln!(
                "[info] escalating tool={} to server for human approval",
                event.tool_name
            );
            let extra = collect_extra(&event.tool_name, &event.tool_input, None);
            escalate_to_server(cli, provider, &event, extra).await?
        }
        ResolvedAction::Delegate(ref command) => {
            eprintln!("[info] delegating tool={} to: {}", event.tool_name, command);
            let delegate_input = serde_json::to_string(&DelegatePayload::from_event(&event))
                .map_err(|e| {
                    RunError::FailClosed(format!("failed to serialise delegate payload: {e}"))
                })?;
            let result = spawn_delegate(command, &delegate_input)
                .await
                .map_err(RunError::FailClosed)?;
            eprintln!(
                "[info] delegate returned: permission={:?} reason={:?}",
                result.permission, result.reason
            );
            match result.permission.as_deref() {
                Some("allow") => HookDecision {
                    status: DecisionStatus::Approved,
                    message: None,
                },
                Some("deny") => HookDecision {
                    status: DecisionStatus::Denied,
                    message: None,
                },
                Some("ask") => {
                    // Delegate wants human review; escalate with original input for context
                    eprintln!(
                        "[info] delegate requested human review, escalating tool={} to server",
                        event.tool_name
                    );
                    let extra = collect_extra(
                        &event.tool_name,
                        &event.tool_input,
                        result.reason.as_deref(),
                    );
                    escalate_to_server(cli, provider, &event, extra).await?
                }
                _ => {
                    // Delegate returned nothing parseable; fall back to config default
                    eprintln!(
                        "[warn] delegate returned unrecognised permission={:?}, falling back to config default={:?}",
                        result.permission, config.default
                    );
                    let fallback = default_to_resolved(&config.default);
                    match fallback {
                        ResolvedAction::Allow => HookDecision {
                            status: DecisionStatus::Approved,
                            message: None,
                        },
                        ResolvedAction::Deny(msg) => HookDecision {
                            status: match msg {
                                Some(m) => DecisionStatus::DeniedWithReason(m),
                                None => DecisionStatus::Denied,
                            },
                            message: None,
                        },
                        ResolvedAction::Ask => {
                            eprintln!(
                                "[info] config default=ask, escalating tool={} to server",
                                event.tool_name
                            );
                            let extra = collect_extra(&event.tool_name, &event.tool_input, None);
                            escalate_to_server(cli, provider, &event, extra).await?
                        }
                        ResolvedAction::Delegate(_) => unreachable!(),
                    }
                }
            }
        }
    };

    eprintln!(
        "[info] final decision for tool={}: {:?}",
        event.tool_name, decision.status
    );

    // If the provider cannot block inline, skip writing output for server-escalated denials —
    // the server was notified for observability but the agent will proceed regardless.
    let output = provider.format_output(&event, decision);
    print!("{output}");
    Ok(())
}

/// Spawn a delegate subprocess, pipe the canonical delegate payload via stdin, and parse the result.
///
/// The input is always in Claude Code wire format (see DelegatePayload), so delegates
/// only need to understand one format regardless of which provider fired the hook.
async fn spawn_delegate(command: &str, input: &str) -> Result<DelegateResult, String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (cmd, cmd_args) = parts.split_first().ok_or("empty delegate command")?;

    let mut child = tokio::process::Command::new(cmd)
        .args(cmd_args.iter())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn delegate '{cmd}': {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("failed to write to delegate stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("delegate command failed: {e}"))?;

    if output.status.code() == Some(2) {
        return Ok(DelegateResult {
            permission: Some("deny".to_string()),
            reason: None,
        });
    }

    if !output.status.success() {
        return Err(format!("delegate exited with status {}", output.status));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("delegate output is not valid UTF-8: {e}"))?;

    let json: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => {
            return Ok(DelegateResult {
                permission: None,
                reason: None,
            });
        }
    };

    // Delegate always receives Claude Code format, so always parse hookSpecificOutput.
    let permission = json
        .get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecision"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let reason = json
        .get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecisionReason"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(DelegateResult { permission, reason })
}

/// Build the `extra` context blob that is sent to the server for human review.
///
/// For FileWrite tools this is a unified diff so the reviewer can see exactly
/// what would change. For shell calls escalated by Dippy the delegate's reason
/// is forwarded. All other tools return None.
fn collect_extra(
    tool_name: &str,
    tool_input: &serde_json::Value,
    dippy_reason: Option<&str>,
) -> Option<serde_json::Value> {
    match tool_name {
        "Write" => {
            let path = tool_input
                .get("path")
                .or_else(|| tool_input.get("file_path"))?
                .as_str()?;
            let new_content = tool_input.get("content")?.as_str()?;
            let old_content = std::fs::read_to_string(path).unwrap_or_default();
            let diff = make_unified_diff(&old_content, new_content, path);
            Some(serde_json::json!({ "diff": diff }))
        }
        "StrReplace" => {
            let old = tool_input.get("old_string")?.as_str()?;
            let new = tool_input.get("new_string")?.as_str()?;
            let path = tool_input
                .get("path")
                .or_else(|| tool_input.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let diff = make_unified_diff(old, new, path);
            Some(serde_json::json!({ "diff": diff }))
        }
        "Edit" => {
            let old = tool_input.get("old_content")?.as_str()?;
            let new = tool_input.get("new_content")?.as_str()?;
            let path = tool_input
                .get("path")
                .or_else(|| tool_input.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let diff = make_unified_diff(old, new, path);
            Some(serde_json::json!({ "diff": diff }))
        }
        "MultiEdit" => {
            let path = tool_input
                .get("path")
                .or_else(|| tool_input.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let edits = tool_input.get("edits")?.as_array()?;
            let combined: String = edits
                .iter()
                .filter_map(|edit| {
                    let old = edit.get("old_string")?.as_str()?;
                    let new = edit.get("new_string")?.as_str()?;
                    Some(make_unified_diff(old, new, path))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if combined.is_empty() {
                return None;
            }
            Some(serde_json::json!({ "diff": combined }))
        }
        _ => {
            if let Some(reason) = dippy_reason {
                return Some(serde_json::json!({ "dippy_reason": reason }));
            }
            None
        }
    }
}

/// Produce a unified diff string from two text blobs.
fn make_unified_diff(old: &str, new: &str, label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- {label}\n+++ {label}\n");
    for group in diff.grouped_ops(3) {
        for op in &group {
            for change in diff.iter_changes(op) {
                let prefix = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(prefix);
                out.push_str(change.value());
                if change.missing_newline() {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// POST an approval request to the server and long-poll until resolved.
async fn escalate_to_server(
    cli: &Cli,
    provider: &dyn Provider,
    event: &ToolHookEvent,
    extra: Option<serde_json::Value>,
) -> Result<HookDecision, RunError> {
    let client = reqwest::Client::new();
    let base = cli.server.trim_end_matches('/');

    let context = ApprovalContext {
        workspace_roots: event.workspace_roots.clone(),
        hook_event_name: event.hook_event_name.clone(),
        extra,
    };

    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    eprintln!(
        "[info] registering approval request: tool={} session={}",
        event.tool_name, event.session_id
    );
    let register_resp = client
        .post(&register_url)
        .bearer_auth(&cli.token)
        .json(&ApprovalRequest {
            id: request_id,
            session_id: event.session_id.clone(),
            session_display_name: event.session_display_name.clone(),
            cwd: event.cwd.clone(),
            tool_name: event.tool_name.clone(),
            tool_input: event.tool_input.clone(),
            provider: provider.name().to_string(),
            request_type: "tool_use",
            context,
        })
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| RunError::ServerUnreachable(format!("failed to register approval: {e}")))?;

    if !register_resp.status().is_success() {
        eprintln!(
            "[error] server returned {} for approval registration",
            register_resp.status()
        );
        return Err(RunError::ServerUnreachable(format!(
            "server returned {} for approval registration",
            register_resp.status()
        )));
    }

    let approval: ApprovalResponse = register_resp.json().await.map_err(|e| {
        RunError::ServerUnreachable(format!("failed to parse approval response: {e}"))
    })?;

    if approval.status_type != "pending" {
        eprintln!(
            "[info] approval immediately resolved: status={}",
            approval.status_type
        );
        return Ok(status_to_decision(
            &approval.status_type,
            approval.message,
            approval.reason,
        ));
    }

    // Long-poll until resolved or global timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cli.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    eprintln!(
        "[info] waiting for approval (id={}, timeout={}s) — approve or deny in the web UI",
        approval.id, cli.timeout
    );

    let mut attempt: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "[error] approval timed out after {}s with no decision — denying",
                cli.timeout
            );
            return Err(RunError::ServerUnreachable(
                "approval timed out".to_string(),
            ));
        }

        attempt += 1;

        let resp = client
            .get(&wait_url)
            .bearer_auth(&cli.token)
            .timeout(Duration::from_secs(60))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[warn] wait request failed (attempt {attempt}, retrying in 2s): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let http_status = resp.status();

        // Non-success HTTP responses that indicate the approval is gone (4xx)
        // are not retryable. Everything else (5xx, network weirdness) is.
        if !http_status.is_success() {
            if http_status.is_client_error() {
                eprintln!(
                    "[error] server returned {http_status} for approval wait — approval may have expired, denying"
                );
                return Err(RunError::ServerUnreachable(format!(
                    "server returned {http_status} for approval wait"
                )));
            }
            eprintln!(
                "[warn] server returned {http_status} for approval wait (attempt {attempt}, retrying in 2s)"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let body = resp.text().await;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[warn] failed to read wait response body (attempt {attempt}, retrying in 2s): {e}"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let wait: WaitResponse = match serde_json::from_str(&body) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
                    "[warn] failed to parse wait response (attempt {attempt}, retrying in 2s): {e} — body: {body}"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if http_status.as_u16() == 202 || wait.status_type == "pending" {
            let secs_remaining = remaining.as_secs();
            eprintln!(
                "[info] approval still pending (attempt {attempt}, {secs_remaining}s remaining)"
            );
            // No sleep — server already held the connection open for its long-poll window.
            continue;
        }

        eprintln!("[info] approval resolved: status={}", wait.status_type);
        return Ok(status_to_decision(
            &wait.status_type,
            wait.message,
            wait.reason,
        ));
    }
}

fn status_to_decision(
    status_type: &str,
    message: Option<String>,
    reason: Option<String>,
) -> HookDecision {
    match status_type {
        "approved" => HookDecision {
            status: DecisionStatus::Approved,
            message,
        },
        _ => {
            let r = reason.or(message);
            HookDecision {
                status: match r {
                    Some(m) => DecisionStatus::DeniedWithReason(m),
                    None => DecisionStatus::Denied,
                },
                message: None,
            }
        }
    }
}

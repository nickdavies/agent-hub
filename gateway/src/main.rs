mod providers;
mod question;
mod types;

use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use config::{ConfigAction, expand_tilde, load_tool_config, resolve_action};
use protocol::{
    ApprovalContext, ApprovalRequest, ApprovalResponse, ApprovalStatus, ApprovalWaitResponse,
    ExtraContext, RequestType, Secret, StatusReport, ToolCallKind,
};
use similar::{ChangeTag, TextDiff};
use uuid::Uuid;

use types::{DecisionStatus, HookDecision, HookOutput, ParseError, ToolHookEvent};

/// Agent Hub gateway — provider-agnostic tool approval hook.
///
/// Exit codes:
///   0 = success (approval decision written to stdout)
///   1 = server unreachable (connection error, timeout); agent should ask the user
///   2 = fail-closed (bad input, config error, policy denial); agent should deny
#[derive(Parser)]
#[command(name = "agent-hub-gateway", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Process a tool-use approval hook (reads hook payload from stdin)
    Approval(ApprovalArgs),
    /// Report session status to the server (reads status JSON from stdin)
    StatusReport(StatusReportArgs),
    /// Proxy a plan-mode question to the server and wait for an answer (reads QuestionProxyRequest JSON from stdin)
    Question(question::QuestionArgs),
}

#[derive(Args)]
struct ApprovalArgs {
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
    token: Secret,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "600")]
    timeout: u64,

    /// Path to tool routing config file (JSON)
    #[arg(long, default_value = "~/.config/agent-hub/tools.json")]
    config: String,
}

#[derive(Args)]
struct StatusReportArgs {
    /// Server URL (e.g. https://hub.example.com)
    #[arg(long, env = "AGENT_HUB_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "AGENT_HUB_TOKEN")]
    token: Secret,
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

    match cli.command {
        Command::Approval(args) => run_approval(args).await,
        Command::StatusReport(args) => run_status_report(args).await,
        Command::Question(args) => question::run(args).await,
    }
}

async fn run_approval(args: ApprovalArgs) -> ExitCode {
    if args.claude {
        run_hook::<protocol::ClaudeCodeHookInput>(
            args,
            "claude-code",
            providers::claude_code::format_output,
        )
        .await
    } else if args.cursor {
        run_hook::<protocol::CursorHookInput>(args, "cursor", providers::cursor::format_output)
            .await
    } else if args.opencode {
        run_hook::<protocol::OpenCodeHookInput>(
            args,
            "opencode",
            providers::opencode::format_output,
        )
        .await
    } else {
        eprintln!("agent-hub-gateway: one of --claude, --cursor, or --opencode is required");
        ExitCode::from(2)
    }
}

/// Generic hook handler: read stdin, deserialize to a provider-specific input type,
/// convert to canonical ToolHookEvent, run the rule engine + approval flow, then
/// format the decision back into the provider's output wire format.
async fn run_hook<I>(
    args: ApprovalArgs,
    provider_name: &str,
    format_output: fn(&ToolHookEvent, &HookOutput) -> String,
) -> ExitCode
where
    I: serde::de::DeserializeOwned + TryInto<ToolHookEvent, Error = ParseError>,
{
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("agent-hub-gateway: failed to read stdin: {e}");
        return ExitCode::from(2);
    }

    let event = match parse_input::<I>(&raw) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("agent-hub-gateway: failed to parse hook payload: {e}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "[info] tool={} session={} cwd={}",
        event.tool_call.tool_name(),
        event.session_id,
        event.cwd
    );

    let config_path = expand_tilde(&args.config);

    match run(&args, provider_name, &event, &config_path).await {
        Ok(decision) => {
            eprintln!(
                "[info] final decision for tool={}: {:?}",
                event.tool_call.tool_name(),
                decision.status
            );
            // If the provider cannot block inline, the server was notified for
            // observability but the agent will proceed regardless.
            let output = format_output(&event, &decision);
            print!("{output}");
            ExitCode::SUCCESS
        }
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

/// Deserialize raw JSON into a provider-specific input type, then convert to
/// the canonical ToolHookEvent.
fn parse_input<I>(raw: &str) -> Result<ToolHookEvent, ParseError>
where
    I: serde::de::DeserializeOwned + TryInto<ToolHookEvent, Error = ParseError>,
{
    let input: I =
        serde_json::from_str(raw).map_err(|e| ParseError(format!("invalid JSON: {e}")))?;
    input.try_into()
}

async fn run_status_report(args: StatusReportArgs) -> ExitCode {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("agent-hub-gateway: failed to read stdin: {e}");
        return ExitCode::from(2);
    }

    // Typed deserialization — catches schema mismatches at the gateway
    let report: StatusReport = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-hub-gateway: invalid status report JSON: {e}");
            return ExitCode::from(2);
        }
    };

    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/hooks/status", args.server.trim_end_matches('/'));

    match client
        .post(&url)
        .bearer_auth(args.token.expose())
        .json(&report)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => ExitCode::SUCCESS,
        Ok(resp) => {
            eprintln!(
                "agent-hub-gateway: status report returned {}",
                resp.status()
            );
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("agent-hub-gateway: status report failed: {e}");
            ExitCode::from(1)
        }
    }
}

// --- Core approval logic ---

async fn run(
    args: &ApprovalArgs,
    provider_name: &str,
    event: &ToolHookEvent,
    config_path: &str,
) -> Result<HookOutput, RunError> {
    let config = load_tool_config(config_path).map_err(RunError::FailClosed)?;

    // Extract matchable args from the typed tool call and resolve paths.
    let tool = event.tool_call.tool();
    let resolved_args: Vec<String> = event
        .tool_call
        .matchable_args()
        .iter()
        .map(|a| config::resolve_path(a, &tool, Some(&event.cwd)))
        .collect();

    let action = resolve_action(
        &config,
        &tool,
        &resolved_args,
        Some(&event.cwd),
        Some(&event.workspace_roots),
    );

    eprintln!("[info] resolved action: {:?}", action);

    // Resolve config action (+ optional delegation) to a HookDecision.
    let hook_decision = match action {
        ConfigAction::Decision(d) => {
            let hd = HookDecision::from(d);
            eprintln!("[info] config decision: {:?}", hd);
            hd
        }
        ConfigAction::Delegation(ref command) => {
            eprintln!(
                "[info] delegating tool={} to: {}",
                event.tool_call.tool_name(),
                command
            );
            let delegate_payload: protocol::DelegatePayload = event.into();
            let delegate_input = serde_json::to_string(&delegate_payload).map_err(|e| {
                RunError::FailClosed(format!("failed to serialise delegate payload: {e}"))
            })?;
            let hd = spawn_delegate(command, &delegate_input)
                .await
                .map_err(RunError::FailClosed)?;
            eprintln!("[info] delegate returned: {:?}", hd);
            hd
        }
    };

    // Act on the decision.
    let output = match hook_decision {
        HookDecision::Allow => {
            eprintln!("[info] allowing tool={}", event.tool_call.tool_name());
            HookOutput {
                status: DecisionStatus::Approved,
                message: None,
            }
        }
        HookDecision::Deny(reason) => {
            eprintln!(
                "[info] denying tool={} reason={:?}",
                event.tool_call.tool_name(),
                reason
            );
            HookOutput {
                status: match reason {
                    Some(r) => DecisionStatus::DeniedWithReason(r),
                    None => DecisionStatus::Denied,
                },
                message: None,
            }
        }
        HookDecision::Ask(delegate_reason) => {
            eprintln!(
                "[info] escalating tool={} to server for human approval",
                event.tool_call.tool_name()
            );
            let extra = collect_extra(&event.tool_call, delegate_reason.as_deref());
            escalate_to_server(args, provider_name, event, extra).await?
        }
    };

    Ok(output)
}

/// Spawn a delegate subprocess, pipe the canonical delegate payload via stdin, and parse the result.
///
/// The input is always in Claude Code wire format (see DelegatePayload), so delegates
/// only need to understand one format regardless of which provider fired the hook.
///
/// Returns a `HookDecision` directly — the delegate's reason is placed on the
/// appropriate variant (`Deny(reason)` or `Ask(reason)`).
async fn spawn_delegate(command: &str, input: &str) -> Result<HookDecision, String> {
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
        return Ok(HookDecision::Deny(None));
    }

    if !output.status.success() {
        return Err(format!("delegate exited with status {}", output.status));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("delegate output is not valid UTF-8: {e}"))?;

    let delegate_output: protocol::DelegateOutput = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("delegate returned invalid JSON: {e}"))?;

    Ok(HookDecision::from(delegate_output))
}

/// Build the `extra` context blob that is sent to the server for human review.
///
/// For FileWrite tools this is a unified diff so the reviewer can see exactly
/// what would change. For shell calls escalated by Dippy the delegate's reason
/// is forwarded. All other tools return None.
fn collect_extra(
    tool_call: &protocol::ToolCall,
    dippy_reason: Option<&str>,
) -> Option<ExtraContext> {
    match tool_call.kind() {
        ToolCallKind::Write { path, content } => {
            let old_content = std::fs::read_to_string(path).unwrap_or_default();
            match content {
                Some(content) => {
                    let diff = make_unified_diff(&old_content, content, path);
                    Some(ExtraContext::Diff { diff })
                }
                None => None,
            }
        }
        ToolCallKind::StrReplace {
            path,
            old_string,
            new_string,
        } => {
            let label = path.as_deref().unwrap_or("file");
            let diff = make_unified_diff(old_string, new_string, label);
            Some(ExtraContext::Diff { diff })
        }
        ToolCallKind::Edit {
            path,
            old_content,
            new_content,
        } => {
            let label = path.as_deref().unwrap_or("file");
            let diff = make_unified_diff(old_content, new_content, label);
            Some(ExtraContext::Diff { diff })
        }
        ToolCallKind::MultiEdit { path, edits } => {
            let label = path.as_deref().unwrap_or("file");
            let combined: String = edits
                .iter()
                .map(|edit| make_unified_diff(&edit.old_string, &edit.new_string, label))
                .collect::<Vec<_>>()
                .join("\n");
            if combined.is_empty() {
                return None;
            }
            Some(ExtraContext::Diff { diff: combined })
        }
        _ => {
            let reason = dippy_reason?;
            Some(ExtraContext::DippyReason {
                dippy_reason: reason.to_string(),
            })
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
    args: &ApprovalArgs,
    provider_name: &str,
    event: &ToolHookEvent,
    extra: Option<ExtraContext>,
) -> Result<HookOutput, RunError> {
    let client = reqwest::Client::new();
    let base = args.server.trim_end_matches('/');

    let context = ApprovalContext {
        workspace_roots: event.workspace_roots.clone(),
        hook_event_name: event.hook_event_name.clone().into(),
        extra,
    };

    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    eprintln!(
        "[info] registering approval request: tool={} session={}",
        event.tool_call.tool_name(),
        event.session_id
    );
    let register_resp = client
        .post(&register_url)
        .bearer_auth(args.token.expose())
        .json(&ApprovalRequest {
            id: request_id,
            session_id: event.session_id.clone(),
            session_display_name: event.session_display_name.clone(),
            cwd: event.cwd.clone(),
            tool: event.tool_call.tool(),
            tool_input: event.tool_call.raw_input().clone(),
            provider: provider_name.to_string(),
            request_type: RequestType::ToolUse,
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

    if matches!(approval.status, ApprovalStatus::Pending) {
        // Fall through to long-poll below
    } else {
        eprintln!(
            "[info] approval immediately resolved: status={:?}",
            approval.status
        );
        return Ok(status_to_output(&approval.status));
    }

    // Long-poll until resolved or global timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    eprintln!(
        "[info] waiting for approval (id={}, timeout={}s) — approve or deny in the web UI",
        approval.id, args.timeout
    );

    let cancel_url = format!("{base}/api/v1/approvals/{}/resolve", approval.id);

    tokio::select! {
        result = poll_for_decision(&client, &wait_url, &args.token, deadline, args.timeout) => result,
        _ = shutdown_signal() => {
            eprintln!("[info] received shutdown signal, cancelling approval {}", approval.id);
            // Best-effort cancel — don't let this block shutdown for long.
            let _ = client
                .post(&cancel_url)
                .bearer_auth(args.token.expose())
                .json(&serde_json::json!({"decision": "cancel"}))
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            Err(RunError::ServerUnreachable("aborted by signal".to_string()))
        }
    }
}

/// Long-poll the server until the approval is resolved or the global timeout expires.
async fn poll_for_decision(
    client: &reqwest::Client,
    wait_url: &str,
    token: &Secret,
    deadline: tokio::time::Instant,
    timeout_secs: u64,
) -> Result<HookOutput, RunError> {
    let mut attempt: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "[error] approval timed out after {}s with no decision — denying",
                timeout_secs
            );
            return Err(RunError::ServerUnreachable(
                "approval timed out".to_string(),
            ));
        }

        attempt += 1;

        let resp = client
            .get(wait_url)
            .bearer_auth(token.expose())
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

        let wait: ApprovalWaitResponse = match serde_json::from_str(&body) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
                    "[warn] failed to parse wait response (attempt {attempt}, retrying in 2s): {e} — body: {body}"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if http_status.as_u16() == 202 || matches!(wait.status, ApprovalStatus::Pending) {
            let secs_remaining = remaining.as_secs();
            eprintln!(
                "[info] approval still pending (attempt {attempt}, {secs_remaining}s remaining)"
            );
            // No sleep — server already held the connection open for its long-poll window.
            continue;
        }

        eprintln!("[info] approval resolved: status={:?}", wait.status);
        return Ok(status_to_output(&wait.status));
    }
}

/// Wait for a shutdown signal (SIGTERM or SIGINT on Unix, Ctrl+C elsewhere).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c handler");
    }
}

fn status_to_output(status: &ApprovalStatus) -> HookOutput {
    match status {
        ApprovalStatus::Approved { message } => HookOutput {
            status: DecisionStatus::Approved,
            message: message.clone(),
        },
        ApprovalStatus::Denied { reason } => HookOutput {
            status: if reason.is_empty() {
                DecisionStatus::Denied
            } else {
                DecisionStatus::DeniedWithReason(reason.clone())
            },
            message: None,
        },
        ApprovalStatus::Cancelled => HookOutput {
            status: DecisionStatus::Denied,
            message: None,
        },
        ApprovalStatus::Pending => HookOutput {
            // Should not happen — but if we get here, deny as a safety net.
            status: DecisionStatus::Denied,
            message: None,
        },
    }
}

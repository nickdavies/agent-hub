mod providers;
mod question;
mod types;

use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use config::{ConfigAction, expand_tilde, load_tool_config, resolve_action_with_input};
use protocol::{
    ApprovalContext, ApprovalRequest, ApprovalResponse, ApprovalStatus, ApprovalWaitResponse,
    ExtraContext, RequestType, Secret, StatusReport, ToolCallKind,
};
use similar::{ChangeTag, TextDiff};
use uuid::Uuid;

use types::{DecisionStatus, HookDecision, HookOutput, ParseError, ToolHookEvent};

type FormatOutput = fn(&ToolHookEvent, &HookOutput) -> String;
type FormatBlockingDenial = fn(&str, &str) -> Option<String>;

/// Agent Hub gateway — provider-agnostic tool approval hook.
///
/// Approval exit codes:
///   0 = provider decision written to stdout
///   2 = blocking hook error when malformed input cannot identify Claude's hook shape
/// Native denial shapes are emitted for parser, policy, delegate, and HTTP failures.
///
/// Other subcommands document their own exit codes.
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
    token: Option<Secret>,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "86400")]
    timeout: u64,

    /// Path to tool routing config file (JSON)
    #[arg(long, default_value = "~/.config/agent-hub/agent-hub-permissions.json")]
    config: String,
}

#[derive(Args)]
struct StatusReportArgs {
    /// Server URL (e.g. https://hub.example.com)
    #[arg(long, env = "AGENT_HUB_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "AGENT_HUB_TOKEN")]
    token: Option<Secret>,
}

// --- Entrypoint ---

enum RunError {
    /// Hard failure — bad input, config error, etc. Agent should deny.
    FailClosed(String),
}

#[derive(Debug, PartialEq)]
enum ApprovalFlowError {
    Timeout,
    Pipeline(String),
}

impl ApprovalFlowError {
    fn into_output(self) -> HookOutput {
        if let Self::Pipeline(reason) = &self {
            write_bounded_stderr(&format!("[error] approvals pipeline failed: {reason}"));
        }
        HookOutput {
            status: match self {
                Self::Timeout => DecisionStatus::TimedOut,
                Self::Pipeline(reason) => {
                    DecisionStatus::PipelineError(types::bounded_gateway_message(&reason))
                }
            },
            message: None,
        }
    }
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
            providers::claude_code::format_blocking_denial,
        )
        .await
    } else if args.cursor {
        run_hook::<protocol::CursorHookInput>(
            args,
            "cursor",
            providers::cursor::format_output,
            providers::cursor::format_blocking_denial,
        )
        .await
    } else if args.opencode {
        run_hook::<protocol::OpenCodeHookInput>(
            args,
            "opencode",
            providers::opencode::format_output,
            providers::opencode::format_blocking_denial,
        )
        .await
    } else {
        write_bounded_stderr(
            "agent-hub-gateway: one of --claude, --cursor, or --opencode is required",
        );
        ExitCode::from(2)
    }
}

/// Generic hook handler: read stdin, deserialize to a provider-specific input type,
/// convert to canonical ToolHookEvent, run the rule engine + approval flow, then
/// format the decision back into the provider's output wire format.
async fn run_hook<I>(
    args: ApprovalArgs,
    provider_name: &str,
    format_output: FormatOutput,
    format_blocking_denial: FormatBlockingDenial,
) -> ExitCode
where
    I: serde::de::DeserializeOwned + TryInto<ToolHookEvent, Error = ParseError>,
{
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        return emit_preparse_denial(
            &raw,
            &format!("failed to read provider input: {e}"),
            format_blocking_denial,
        );
    }

    let event = match parse_input::<I>(&raw) {
        Ok(e) => e,
        Err(e) => {
            return emit_preparse_denial(
                &raw,
                &format!("failed to parse provider input: {e}"),
                format_blocking_denial,
            );
        }
    };

    if let Err(error) = gateway_http_client(&args.server) {
        return emit_provider_denial(&event, &error, format_output);
    }

    write_bounded_stderr(&format!(
        "[info] tool={} session={} cwd={}",
        event.tool_call.tool_name(),
        event.session_id,
        event.cwd
    ));

    let config_path = expand_tilde(&args.config);

    match run(&args, provider_name, &event, &config_path).await {
        Ok(decision) => {
            write_bounded_stderr(&format!(
                "[info] final decision for tool={}: {}",
                event.tool_call.tool_name(),
                decision_status_kind(&decision.status)
            ));
            let output = format_output(&event, &decision);
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(RunError::FailClosed(e)) => emit_provider_denial(&event, &e, format_output),
    }
}

fn emit_preparse_denial(
    raw: &str,
    detail: &str,
    format_blocking_denial: FormatBlockingDenial,
) -> ExitCode {
    let reason = types::approval_pipeline_error_message(detail);
    write_bounded_stderr(&format!("agent-hub-gateway: {reason}"));
    match format_blocking_denial(raw, &reason) {
        Some(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        None => ExitCode::from(2),
    }
}

fn emit_provider_denial(
    event: &ToolHookEvent,
    detail: &str,
    format_output: FormatOutput,
) -> ExitCode {
    let detail = types::bounded_gateway_message(detail);
    write_bounded_stderr(&format!("agent-hub-gateway: {detail}"));
    let decision = HookOutput {
        status: DecisionStatus::PipelineError(detail),
        message: None,
    };
    print!("{}", format_output(event, &decision));
    ExitCode::SUCCESS
}

fn write_bounded_stderr(message: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static WRITTEN: AtomicUsize = AtomicUsize::new(0);
    let mut written = WRITTEN.load(Ordering::Relaxed);
    loop {
        let remaining = types::MAX_GATEWAY_DIAGNOSTIC_BYTES.saturating_sub(written);
        if remaining == 0 {
            return;
        }
        let line = types::bounded_message(&format!("{message}\n"), remaining);
        if line.is_empty() {
            return;
        }
        match WRITTEN.compare_exchange_weak(
            written,
            written + line.len(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                eprint!("{line}");
                return;
            }
            Err(actual) => written = actual,
        }
    }
}

fn hook_decision_kind(decision: &HookDecision) -> &'static str {
    match decision {
        HookDecision::Allow => "allow",
        HookDecision::Deny(_) => "deny",
        HookDecision::Ask(_) => "ask",
    }
}

fn decision_status_kind(status: &DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Approved => "approved",
        DecisionStatus::Denied | DecisionStatus::DeniedWithReason(_) => "denied",
        DecisionStatus::TimedOut => "timed_out",
        DecisionStatus::PipelineError(_) => "pipeline_error",
    }
}

fn approval_status_kind(status: &ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Approved { .. } => "approved",
        ApprovalStatus::Denied { .. } => "denied",
        ApprovalStatus::Cancelled => "cancelled",
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
        write_bounded_stderr(&format!("agent-hub-gateway: failed to read stdin: {e}"));
        return ExitCode::from(2);
    }

    // Typed deserialization — catches schema mismatches at the gateway
    let report: StatusReport = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            write_bounded_stderr(&format!(
                "agent-hub-gateway: invalid status report JSON: {e}"
            ));
            return ExitCode::from(2);
        }
    };

    let (client, base) = match gateway_http_client(&args.server) {
        Ok(value) => value,
        Err(error) => {
            write_bounded_stderr(&format!("agent-hub-gateway: {error}"));
            return ExitCode::from(2);
        }
    };
    let url = format!(
        "{}/api/v1/hooks/status",
        base.as_str().trim_end_matches('/')
    );

    match authenticated(args.token.as_ref(), client.post(&url))
        .json(&report)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => ExitCode::SUCCESS,
        Ok(resp) => {
            write_bounded_stderr(&format!(
                "agent-hub-gateway: status report returned {}",
                resp.status()
            ));
            ExitCode::from(1)
        }
        Err(e) => {
            write_bounded_stderr(&format!("agent-hub-gateway: status report failed: {e}"));
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

    let action = resolve_action_with_input(
        &config,
        &tool,
        &resolved_args,
        event.tool_call.policy_input(),
        Some(&event.cwd),
        Some(&event.workspace_roots),
    );

    write_bounded_stderr(&format!(
        "[info] resolved action: {}",
        match &action {
            ConfigAction::Decision(_) => "decision",
            ConfigAction::Delegation(_) => "delegation",
        }
    ));

    // Resolve config action (+ optional delegation) to a HookDecision.
    let hook_decision = match action {
        ConfigAction::Decision(d) => {
            let hd = HookDecision::from(d);
            write_bounded_stderr(&format!(
                "[info] config decision: {}",
                hook_decision_kind(&hd)
            ));
            hd
        }
        ConfigAction::Delegation(ref command) => {
            write_bounded_stderr(&format!(
                "[info] delegating tool={}",
                event.tool_call.tool_name()
            ));
            let delegate_payload: protocol::DelegatePayload = event.into();
            let delegate_input = serde_json::to_string(&delegate_payload).map_err(|e| {
                RunError::FailClosed(format!("failed to serialise delegate payload: {e}"))
            })?;
            let hd = spawn_delegate(command, &delegate_input)
                .await
                .map_err(RunError::FailClosed)?;
            write_bounded_stderr(&format!(
                "[info] delegate returned: {}",
                hook_decision_kind(&hd)
            ));
            hd
        }
    };

    // Act on the decision.
    let output = match hook_decision {
        HookDecision::Allow => {
            write_bounded_stderr(&format!(
                "[info] allowing tool={}",
                event.tool_call.tool_name()
            ));
            HookOutput {
                status: DecisionStatus::Approved,
                message: None,
            }
        }
        HookDecision::Deny(reason) => {
            write_bounded_stderr(&format!(
                "[info] denying tool={} reason_present={}",
                event.tool_call.tool_name(),
                reason.is_some()
            ));
            HookOutput {
                status: match reason {
                    Some(r) => DecisionStatus::DeniedWithReason(r),
                    None => DecisionStatus::Denied,
                },
                message: None,
            }
        }
        HookDecision::Ask(delegate_reason) => {
            write_bounded_stderr(&format!(
                "[info] escalating tool={} to server for human approval",
                event.tool_call.tool_name()
            ));
            let extra = collect_extra(&event.tool_call, delegate_reason.as_deref());
            match escalate_to_server(args, provider_name, event, extra).await {
                Ok(output) => output,
                Err(error) => error.into_output(),
            }
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
        .stderr(std::process::Stdio::piped())
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
        return Err(types::bounded_gateway_message(&format!(
            "delegate exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
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
) -> Result<HookOutput, ApprovalFlowError> {
    let (client, base) = gateway_http_client(&args.server).map_err(ApprovalFlowError::Pipeline)?;
    let base = base.as_str().trim_end_matches('/');

    let context = ApprovalContext {
        workspace_roots: event.workspace_roots.clone(),
        hook_event_name: event.hook_event_name.clone().into(),
        extra,
    };

    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    write_bounded_stderr(&format!(
        "[info] registering approval request: tool={} session={}",
        event.tool_call.tool_name(),
        event.session_id
    ));
    let register_resp = authenticated(args.token.as_ref(), client.post(&register_url))
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
        .map_err(|e| ApprovalFlowError::Pipeline(format!("failed to register approval: {e}")))?;

    if !register_resp.status().is_success() {
        write_bounded_stderr(&format!(
            "[error] server returned {} for approval registration",
            register_resp.status()
        ));
        return Err(ApprovalFlowError::Pipeline(format!(
            "server returned {} for approval registration",
            register_resp.status()
        )));
    }

    let body = read_response_body(register_resp)
        .await
        .map_err(ApprovalFlowError::Pipeline)?;
    let approval: ApprovalResponse = serde_json::from_str(&body).map_err(|e| {
        ApprovalFlowError::Pipeline(format!("failed to parse approval response: {e}"))
    })?;
    validate_approval_status(&approval.status)?;

    if matches!(approval.status, ApprovalStatus::Pending) {
        // Fall through to long-poll below
    } else {
        write_bounded_stderr(&format!(
            "[info] approval immediately resolved: status={}",
            approval_status_kind(&approval.status)
        ));
        return Ok(status_to_output(&approval.status));
    }

    // Long-poll until resolved or global timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    write_bounded_stderr(&format!(
        "[info] waiting for approval (id={}, timeout={}s) — approve or deny in the web UI",
        approval.id, args.timeout
    ));

    let cancel_url = format!("{base}/api/v1/approvals/{}/resolve", approval.id);

    tokio::select! {
        result = poll_for_decision(&client, &wait_url, args.token.as_ref(), deadline, args.timeout) => result,
        _ = shutdown_signal() => {
            write_bounded_stderr(&format!("[info] received shutdown signal, cancelling approval {}", approval.id));
            // Best-effort cancel — don't let this block shutdown for long.
            let _ = authenticated(args.token.as_ref(), client.post(&cancel_url))
                .json(&serde_json::json!({"decision": "cancel"}))
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            Err(ApprovalFlowError::Pipeline("aborted by signal".to_string()))
        }
    }
}

/// Long-poll the server until the approval is resolved or the global timeout expires.
async fn poll_for_decision(
    client: &reqwest::Client,
    wait_url: &str,
    token: Option<&Secret>,
    deadline: tokio::time::Instant,
    timeout_secs: u64,
) -> Result<HookOutput, ApprovalFlowError> {
    let mut attempt: u32 = 0;
    let mut last_wait_was_healthy = true;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            write_bounded_stderr(&format!(
                "[error] approval timed out after {timeout_secs}s with no decision"
            ));
            return Err(deadline_error(last_wait_was_healthy));
        }

        attempt += 1;

        let resp = await_approval_deadline(
            deadline,
            last_wait_was_healthy,
            authenticated(token, client.get(wait_url))
                .timeout(Duration::from_secs(60))
                .send(),
        )
        .await?;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                write_bounded_stderr(&format!(
                    "[warn] wait request failed (attempt {attempt}, retrying in 2s): {e}"
                ));
                last_wait_was_healthy = false;
                await_approval_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        let http_status = resp.status();

        // Non-success HTTP responses that indicate the approval is gone (4xx)
        // are not retryable. Everything else (5xx, network weirdness) is.
        if !http_status.is_success() {
            if http_status.is_client_error() {
                write_bounded_stderr(&format!(
                    "[error] server returned {http_status} for approval wait — approval may have expired"
                ));
                return Err(ApprovalFlowError::Pipeline(format!(
                    "server returned {http_status} for approval wait"
                )));
            }
            write_bounded_stderr(&format!(
                "[warn] server returned {http_status} for approval wait (attempt {attempt}, retrying in 2s)"
            ));
            last_wait_was_healthy = false;
            await_approval_deadline(
                deadline,
                last_wait_was_healthy,
                tokio::time::sleep(Duration::from_secs(2)),
            )
            .await?;
            continue;
        }

        let body =
            await_approval_deadline(deadline, last_wait_was_healthy, read_response_body(resp))
                .await?;
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                write_bounded_stderr(&format!(
                    "[warn] failed to read wait response body (attempt {attempt}, retrying in 2s): {e}"
                ));
                last_wait_was_healthy = false;
                await_approval_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        let wait: ApprovalWaitResponse = match serde_json::from_str(&body) {
            Ok(w) => w,
            Err(e) => {
                write_bounded_stderr(&format!(
                    "[warn] failed to parse wait response (attempt {attempt}, retrying in 2s): {e}; body_bytes={}",
                    body.len()
                ));
                last_wait_was_healthy = false;
                await_approval_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        validate_approval_status(&wait.status)?;
        ensure_approval_before_deadline(deadline, last_wait_was_healthy)?;

        if http_status.as_u16() == 202 {
            if !matches!(wait.status, ApprovalStatus::Pending) {
                return Err(ApprovalFlowError::Pipeline(format!(
                    "server returned {http_status} with non-pending approval status"
                )));
            }
            last_wait_was_healthy = true;
            let secs_remaining = remaining.as_secs();
            write_bounded_stderr(&format!(
                "[info] approval still pending (attempt {attempt}, {secs_remaining}s remaining)"
            ));
            // No sleep — server already held the connection open for its long-poll window.
            continue;
        }

        if matches!(wait.status, ApprovalStatus::Pending) {
            return Err(ApprovalFlowError::Pipeline(
                "server returned an unexpected final pending approval status".to_string(),
            ));
        }

        write_bounded_stderr(&format!(
            "[info] approval resolved: status={}",
            approval_status_kind(&wait.status)
        ));
        return Ok(status_to_output(&wait.status));
    }
}

fn authenticated(
    token: Option<&Secret>,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token.expose()),
        None => request,
    }
}

const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;

fn validate_endpoint(value: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(value).map_err(|e| format!("server URL is invalid: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("server URL must use HTTP or HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("server URL must not contain credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("server URL must not contain query or fragment".into());
    }
    url.host_str().ok_or("server URL has no host")?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

fn gateway_http_client(server: &str) -> Result<(reqwest::Client, reqwest::Url), String> {
    let endpoint = validate_endpoint(server)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;
    Ok((client, endpoint))
}

async fn read_response_body(mut response: reqwest::Response) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
    {
        return Err("response exceeds limit".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("failed to read response: {e}"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
            return Err("response exceeds limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|e| format!("response is not UTF-8: {e}"))
}

fn validate_approval_status(status: &ApprovalStatus) -> Result<(), ApprovalFlowError> {
    let reason = match status {
        ApprovalStatus::Approved { message } => message.as_deref(),
        ApprovalStatus::Denied { reason } => Some(reason.as_str()),
        ApprovalStatus::Pending | ApprovalStatus::Cancelled => None,
    };
    if reason.is_some_and(|reason| {
        reason.is_empty() || reason.len() > types::MAX_GATEWAY_DIAGNOSTIC_BYTES
    }) {
        return Err(ApprovalFlowError::Pipeline(
            "approval response reason exceeds required bounds".to_string(),
        ));
    }
    Ok(())
}

fn deadline_error(last_wait_was_healthy: bool) -> ApprovalFlowError {
    if last_wait_was_healthy {
        ApprovalFlowError::Timeout
    } else {
        ApprovalFlowError::Pipeline("approval wait failed until the deadline".to_string())
    }
}

fn ensure_approval_before_deadline(
    deadline: tokio::time::Instant,
    last_wait_was_healthy: bool,
) -> Result<(), ApprovalFlowError> {
    if tokio::time::Instant::now() >= deadline {
        Err(deadline_error(last_wait_was_healthy))
    } else {
        Ok(())
    }
}

async fn await_approval_deadline<F>(
    deadline: tokio::time::Instant,
    last_wait_was_healthy: bool,
    future: F,
) -> Result<F::Output, ApprovalFlowError>
where
    F: std::future::Future,
{
    ensure_approval_before_deadline(deadline, last_wait_was_healthy)?;
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| deadline_error(last_wait_was_healthy))
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
            status: DecisionStatus::PipelineError(
                "approval was cancelled without a decision".into(),
            ),
            message: None,
        },
        ApprovalStatus::Pending => HookOutput {
            status: DecisionStatus::PipelineError("approval remained pending unexpectedly".into()),
            message: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_status_uses_the_historical_optional_nonempty_byte_bounded_message_contract() {
        for status in [
            ApprovalStatus::Pending,
            ApprovalStatus::Cancelled,
            ApprovalStatus::Approved { message: None },
            ApprovalStatus::Approved {
                message: Some(" ".into()),
            },
            ApprovalStatus::Approved {
                message: Some("x".repeat(4096)),
            },
            ApprovalStatus::Denied {
                reason: format!("{}x", "界".repeat(1365)),
            },
        ] {
            assert!(validate_approval_status(&status).is_ok());
        }
        for reason in [String::new(), "x".repeat(4097), "界".repeat(1366)] {
            for status in [
                ApprovalStatus::Approved {
                    message: Some(reason.clone()),
                },
                ApprovalStatus::Denied {
                    reason: reason.clone(),
                },
            ] {
                assert!(validate_approval_status(&status).is_err());
            }
        }
    }

    #[test]
    fn host_endpoints_accept_http_without_a_loopback_or_token_restriction() {
        for endpoint in [
            "http://0.0.0.0:8080",
            "http://localhost:8080",
            "http://example.test/base",
            "https://example.test",
            "http://[::1]:8080",
        ] {
            assert!(validate_endpoint(endpoint).is_ok());
        }
        for endpoint in [
            "ftp://example.test",
            "http://user:password@example.test",
            "https://example.test?query=value",
            "https://example.test/#fragment",
            "not a URL",
        ] {
            assert!(validate_endpoint(endpoint).is_err());
        }
    }

    #[test]
    fn approval_default_timeout_is_24_hours() {
        let cli = Cli::try_parse_from([
            "agent-hub-gateway",
            "approval",
            "--server",
            "http://localhost",
            "--token",
            "test-token",
        ])
        .unwrap();

        let Command::Approval(args) = cli.command else {
            panic!("expected approval command");
        };
        assert_eq!(args.timeout, 86_400);
    }

    #[test]
    fn question_default_timeout_is_24_hours() {
        let cli = Cli::try_parse_from([
            "agent-hub-gateway",
            "question",
            "--server",
            "http://localhost",
            "--token",
            "test-token",
        ])
        .unwrap();

        let Command::Question(args) = cli.command else {
            panic!("expected question command");
        };
        assert_eq!(args.timeout, 86_400);
    }

    #[test]
    fn explicit_denial_remains_denied() {
        let output = status_to_output(&ApprovalStatus::Denied {
            reason: "operator rejected this".to_string(),
        });
        assert!(matches!(
            output.status,
            DecisionStatus::DeniedWithReason(ref reason) if reason == "operator rejected this"
        ));
    }

    #[test]
    fn cancelled_and_unexpected_pending_are_pipeline_errors() {
        for status in [ApprovalStatus::Cancelled, ApprovalStatus::Pending] {
            assert!(matches!(
                status_to_output(&status).status,
                DecisionStatus::PipelineError(_)
            ));
        }
    }

    #[test]
    fn healthy_pending_deadline_is_timeout_but_failed_wait_is_pipeline_error() {
        assert_eq!(deadline_error(true), ApprovalFlowError::Timeout);
        assert!(matches!(
            deadline_error(false),
            ApprovalFlowError::Pipeline(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn approval_deadline_rejects_late_response_after_healthy_pending() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = await_approval_deadline(deadline, true, async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            HookOutput {
                status: DecisionStatus::Approved,
                message: None,
            }
        })
        .await;

        assert_eq!(result.unwrap_err(), ApprovalFlowError::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn approval_deadline_after_recurring_failure_is_pipeline_error() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = await_approval_deadline(deadline, false, async {
            tokio::time::sleep(Duration::from_secs(6)).await;
        })
        .await;

        assert!(matches!(result, Err(ApprovalFlowError::Pipeline(_))));
    }
}

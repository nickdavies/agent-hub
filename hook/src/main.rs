use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Approval hook for Claude Code and Cursor.
///
/// Exit codes:
///   0 = success (output on stdout if decision made, no output for fall-through)
///   2 = block (error occurred, fail-closed)
#[derive(Parser)]
#[command(name = "claude-approve", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Standalone approval hook — checks session mode and requests remote approval
    Approve(ApproveArgs),
    /// Route tool calls via config file — delegates, auto-approves, or requests remote approval
    Delegate(DelegateArgs),
}

#[derive(Args)]
struct SharedArgs {
    /// Server URL (e.g. https://notify.example.com)
    #[arg(long, env = "CLAUDE_NOTIFY_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "CLAUDE_NOTIFY_TOKEN")]
    token: String,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "600")]
    timeout: u64,

    /// Output format
    #[arg(long)]
    format: OutputFormat,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Cursor,
    Claude,
}

#[derive(Args)]
struct ApproveArgs {
    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Args)]
struct DelegateArgs {
    #[command(flatten)]
    shared: SharedArgs,

    /// Path to tool routing config file (JSON)
    #[arg(long)]
    config: String,
}

// --- Wire types ---

/// Minimal hook payload (stdin). Accepts both Claude Code and Cursor field names.
#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
    conversation_id: Option<String>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    cwd: Option<String>,
}

impl HookInput {
    fn session_id(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .or(self.conversation_id.as_deref())
    }
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

// --- Tool routing config ---

// JSON deserialization types

#[derive(Deserialize)]
struct ToolConfigJson {
    #[allow(dead_code)]
    version: u32,
    default: DefaultAction,
    rules: Vec<RuleJson>,
}

#[derive(Deserialize)]
struct RuleJson {
    tools: Vec<String>,
    action: RuleAction,
    command: Option<String>,
    message: Option<String>,
}

// Compiled config types

#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum DefaultAction {
    Allow,
    Deny,
    Ask,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuleAction {
    Allow,
    Deny,
    Ask,
    Delegate,
}

enum ToolPattern {
    Glob(globset::GlobMatcher),
    Regex(regex::Regex),
}

impl ToolPattern {
    fn is_match(&self, value: &str) -> bool {
        match self {
            ToolPattern::Glob(g) => g.is_match(value),
            ToolPattern::Regex(r) => r.is_match(value),
        }
    }
}

struct ToolMatcher {
    name: String,
    pattern: Option<ToolPattern>,
}

impl ToolMatcher {
    fn matches(
        &self,
        tool_name: &str,
        tool_input: Option<&serde_json::Value>,
        cwd: Option<&str>,
    ) -> bool {
        if self.name != tool_name {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(pattern) => {
                let arg = tool_input.and_then(|input| get_matchable_arg(tool_name, input, cwd));
                match arg {
                    Some(ref value) => pattern.is_match(value),
                    None => false,
                }
            }
        }
    }
}

struct Rule {
    matchers: Vec<ToolMatcher>,
    action: RuleAction,
    command: Option<String>,
    message: Option<String>,
}

struct ToolConfig {
    default: DefaultAction,
    rules: Vec<Rule>,
}

#[derive(Debug)]
enum ResolvedAction {
    Allow,
    Deny(Option<String>),
    Ask,
    Delegate(String),
}

/// Parse a tool entry like "Write", "Write(src/**)", or "Write(regex:\.env)".
fn parse_tool_entry(entry: &str) -> Result<ToolMatcher, String> {
    if let Some(paren_start) = entry.find('(') {
        if !entry.ends_with(')') {
            return Err(format!(
                "malformed tool pattern (missing closing paren): {entry}"
            ));
        }
        let name = &entry[..paren_start];
        let pat = &entry[paren_start + 1..entry.len() - 1];
        if name.is_empty() || pat.is_empty() {
            return Err(format!("empty tool name or pattern: {entry}"));
        }
        let pattern = if let Some(regex_pat) = pat.strip_prefix("regex:") {
            let re = regex::Regex::new(regex_pat)
                .map_err(|e| format!("invalid regex in '{entry}': {e}"))?;
            ToolPattern::Regex(re)
        } else {
            let glob = globset::Glob::new(pat)
                .map_err(|e| format!("invalid glob in '{entry}': {e}"))?
                .compile_matcher();
            ToolPattern::Glob(glob)
        };
        Ok(ToolMatcher {
            name: name.to_string(),
            pattern: Some(pattern),
        })
    } else {
        Ok(ToolMatcher {
            name: entry.to_string(),
            pattern: None,
        })
    }
}

fn is_path_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Read"
            | "Write"
            | "StrReplace"
            | "Delete"
            | "Edit"
            | "MultiEdit"
            | "EditNotebook"
            | "NotebookEdit"
    )
}

/// Extract the argument to match against from tool_input, based on tool type.
/// For path-based tools, relative paths are resolved against cwd.
fn get_matchable_arg(
    tool_name: &str,
    tool_input: &serde_json::Value,
    cwd: Option<&str>,
) -> Option<String> {
    let raw = match tool_name {
        "Read" | "Write" | "StrReplace" | "Delete" | "Edit" | "MultiEdit" => tool_input
            .get("path")
            .or_else(|| tool_input.get("file_path"))
            .and_then(|v| v.as_str()),
        "EditNotebook" | "NotebookEdit" => {
            tool_input.get("target_notebook").and_then(|v| v.as_str())
        }
        "Bash" | "Shell" => tool_input.get("command").and_then(|v| v.as_str()),
        "WebFetch" => tool_input.get("url").and_then(|v| v.as_str()),
        _ => None,
    }?;

    if is_path_tool(tool_name) {
        if let Some(cwd) = cwd {
            let path = std::path::Path::new(raw);
            if path.is_relative() {
                return Some(
                    std::path::Path::new(cwd)
                        .join(path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }

    Some(raw.to_string())
}

fn load_tool_config(path: &str) -> Result<ToolConfig, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config {path}: {e}"))?;
    let raw: ToolConfigJson = serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse config {path}: {e}"))?;

    let mut rules = Vec::with_capacity(raw.rules.len());
    for raw_rule in raw.rules {
        if matches!(raw_rule.action, RuleAction::Delegate) && raw_rule.command.is_none() {
            return Err(format!(
                "rule for {:?} has action 'delegate' but no 'command'",
                raw_rule.tools
            ));
        }
        let matchers = raw_rule
            .tools
            .iter()
            .map(|t| parse_tool_entry(t))
            .collect::<Result<Vec<_>, _>>()?;
        rules.push(Rule {
            matchers,
            action: raw_rule.action,
            command: raw_rule.command,
            message: raw_rule.message,
        });
    }

    Ok(ToolConfig {
        default: raw.default,
        rules,
    })
}

fn resolve_action(
    config: &ToolConfig,
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: Option<&str>,
) -> ResolvedAction {
    // For path-based tools, deny if the resolved path is not absolute
    if is_path_tool(tool_name) {
        if let Some(input) = tool_input {
            if let Some(ref resolved) = get_matchable_arg(tool_name, input, cwd) {
                if !resolved.starts_with('/') {
                    return ResolvedAction::Deny(Some(
                        "path-based tool arguments must be absolute paths".to_string(),
                    ));
                }
            }
        }
    }

    for rule in &config.rules {
        if rule
            .matchers
            .iter()
            .any(|m| m.matches(tool_name, tool_input, cwd))
        {
            return match &rule.action {
                RuleAction::Allow => ResolvedAction::Allow,
                RuleAction::Deny => ResolvedAction::Deny(rule.message.clone()),
                RuleAction::Ask => ResolvedAction::Ask,
                RuleAction::Delegate => ResolvedAction::Delegate(rule.command.clone().unwrap()),
            };
        }
    }
    default_to_resolved(&config.default)
}

fn default_to_resolved(default: &DefaultAction) -> ResolvedAction {
    match default {
        DefaultAction::Allow => ResolvedAction::Allow,
        DefaultAction::Deny => ResolvedAction::Deny(None),
        DefaultAction::Ask => ResolvedAction::Ask,
    }
}

// --- Entrypoint ---

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Approve(args) => run_approve(&args).await,
        Command::Delegate(args) => run_delegate(&args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claude-approve: {e}");
            ExitCode::from(2)
        }
    }
}

// --- Approve subcommand ---

async fn run_approve(args: &ApproveArgs) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let payload: HookInput =
        serde_json::from_str(&input).map_err(|e| format!("failed to parse hook payload: {e}"))?;

    let session_id = payload
        .session_id()
        .ok_or("missing session_id/conversation_id in payload")?;

    let client = reqwest::Client::new();
    let base = args.shared.server.trim_end_matches('/');

    // Check session approval mode
    let mode_url = format!("{base}/api/v1/sessions/{session_id}/approval-mode");
    let mode_resp = client
        .get(&mode_url)
        .bearer_auth(&args.shared.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to check approval mode: {e}"))?;

    if !mode_resp.status().is_success() {
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
            // Fall through: exit 0 with no output -> host shows normal dialog
            return Ok(());
        }
    }

    // Remote mode: register approval and wait
    let (status_type, message, reason) =
        request_remote_approval(&client, &args.shared, &payload).await?;

    let hook_event = payload.hook_event_name.as_deref().unwrap_or("PreToolUse");
    let output = format_output(
        &args.shared.format,
        hook_event,
        &status_type,
        message.as_deref(),
        reason.as_deref(),
    )?;
    print!("{output}");
    Ok(())
}

// --- Delegate subcommand ---

async fn run_delegate(args: &DelegateArgs) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let payload: HookInput =
        serde_json::from_str(&input).map_err(|e| format!("failed to parse hook payload: {e}"))?;

    let config = load_tool_config(&args.config)?;
    let tool_name = payload.tool_name.as_deref().unwrap_or("unknown");
    let action = resolve_action(
        &config,
        tool_name,
        payload.tool_input.as_ref(),
        payload.cwd.as_deref(),
    );

    match action {
        ResolvedAction::Allow | ResolvedAction::Deny(_) | ResolvedAction::Ask => {
            execute_simple_action(&action, &args.shared, &payload).await
        }
        ResolvedAction::Delegate(ref command) => {
            let result = spawn_delegate(command, &input, &args.shared.format).await?;

            match result.permission.as_deref() {
                Some("allow") | Some("deny") => {
                    print!("{}", result.raw_output);
                    Ok(())
                }
                Some("ask") => {
                    execute_simple_action(&ResolvedAction::Ask, &args.shared, &payload).await
                }
                None => {
                    let fallback = default_to_resolved(&config.default);
                    execute_simple_action(&fallback, &args.shared, &payload).await
                }
                Some(other) => Err(format!("unexpected permission from delegate: {other}")),
            }
        }
    }
}

/// Execute a non-delegate action (allow, deny, or remote approval).
async fn execute_simple_action(
    action: &ResolvedAction,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(), String> {
    let hook_event = payload.hook_event_name.as_deref().unwrap_or("PreToolUse");
    match action {
        ResolvedAction::Allow => {
            let output = format_output(&shared.format, hook_event, "approved", None, None)?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Deny(msg) => {
            let reason = msg.as_deref().unwrap_or("denied by policy");
            let output = format_output(
                &shared.format,
                hook_event,
                "denied",
                None,
                Some(reason),
            )?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Ask => {
            let client = reqwest::Client::new();
            let (status_type, message, reason) =
                request_remote_approval_with_retry(&client, shared, payload).await?;
            let output = format_output(
                &shared.format,
                hook_event,
                &status_type,
                message.as_deref(),
                reason.as_deref(),
            )?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Delegate(_) => unreachable!(),
    }
}

struct DelegateResult {
    permission: Option<String>,
    raw_output: String,
}

/// Spawn a delegate command, pipe hook input via stdin, and parse the result.
async fn spawn_delegate(
    command: &str,
    input: &str,
    format: &OutputFormat,
) -> Result<DelegateResult, String> {
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

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("delegate output is not valid UTF-8: {e}"))?;

    if output.status.code() == Some(2) {
        return Ok(DelegateResult {
            permission: Some("deny".to_string()),
            raw_output: stdout,
        });
    }

    if !output.status.success() {
        return Err(format!("delegate exited with status {}", output.status));
    }

    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse delegate output: {e}"))?;

    let permission = extract_permission(format, &json).map(String::from);

    Ok(DelegateResult {
        permission,
        raw_output: stdout,
    })
}

/// Extract the permission decision string from delegate output based on format.
fn extract_permission<'a>(format: &OutputFormat, json: &'a serde_json::Value) -> Option<&'a str> {
    match format {
        OutputFormat::Cursor => json.get("permission").and_then(|v| v.as_str()),
        OutputFormat::Claude => json
            .get("hookSpecificOutput")
            .and_then(|h| h.get("permissionDecision"))
            .and_then(|v| v.as_str()),
    }
}

// --- Shared approval logic ---

/// Register an approval request with the server and long-poll until resolved.
async fn request_remote_approval(
    client: &reqwest::Client,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(String, Option<String>, Option<String>), String> {
    let base = shared.server.trim_end_matches('/');
    let session_id = payload
        .session_id()
        .ok_or("missing session_id/conversation_id")?;
    let tool_name = payload.tool_name.as_deref().unwrap_or("unknown");
    let tool_input = payload
        .tool_input
        .clone()
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let cwd = payload.cwd.as_deref().unwrap_or(".");

    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    let register_resp = client
        .post(&register_url)
        .bearer_auth(&shared.token)
        .json(&ApprovalRequest {
            request_id,
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            tool_name: tool_name.to_string(),
            tool_input,
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

    if approval.status_type != "pending" {
        return Ok((approval.status_type, approval.message, approval.reason));
    }

    // Long-poll until resolved or timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(shared.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("approval timed out".to_string());
        }

        let resp = client
            .get(&wait_url)
            .bearer_auth(&shared.token)
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
            continue;
        }

        return Ok((wait.status_type, wait.message, wait.reason));
    }
}

/// Wrapper with retry for transient failures (3 attempts, 2s between).
async fn request_remote_approval_with_retry(
    client: &reqwest::Client,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(String, Option<String>, Option<String>), String> {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        match request_remote_approval(client, shared, payload).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_err = e;
                if attempt < MAX_ATTEMPTS {
                    eprintln!(
                        "claude-approve: attempt {attempt}/{MAX_ATTEMPTS} failed: {last_err}, retrying in 2s"
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    Err(format!("failed after {MAX_ATTEMPTS} attempts: {last_err}"))
}

// --- Output formatting ---

/// Format the approval server response as hook output JSON.
fn format_output(
    format: &OutputFormat,
    hook_event: &str,
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    match format {
        OutputFormat::Cursor => format_cursor_output(status_type, message, reason),
        OutputFormat::Claude => format_claude_output(hook_event, status_type, message, reason),
    }
}

fn format_cursor_output(
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    let perm = match status_type {
        "approved" => "allow",
        "denied" => "deny",
        other => return Err(format!("unexpected approval status: {other}")),
    };
    let msg = reason
        .or(message)
        .unwrap_or("resolved via remote approval");
    let output = serde_json::json!({
        "permission": perm,
        "user_message": msg,
        "agent_message": msg,
    });
    serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
}

fn format_claude_output(
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
                "denied" => (
                    "deny",
                    reason.or(message).unwrap_or("denied via remote approval"),
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_tool_entry ---

    #[test]
    fn parse_bare_name() {
        let m = parse_tool_entry("Write").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_none());
    }

    #[test]
    fn parse_glob_pattern() {
        let m = parse_tool_entry("Write(src/**)").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_some());
    }

    #[test]
    fn parse_regex_pattern() {
        let m = parse_tool_entry(r"Write(regex:\.env)").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_some());
    }

    #[test]
    fn parse_malformed_entries() {
        assert!(parse_tool_entry("Write(src/**").is_err());
        assert!(parse_tool_entry("(src/**)").is_err());
        assert!(parse_tool_entry("Write()").is_err());
    }

    #[test]
    fn parse_invalid_regex() {
        assert!(parse_tool_entry("Write(regex:[invalid)").is_err());
    }

    // --- ToolMatcher::matches ---

    #[test]
    fn bare_name_matches_any_input() {
        let m = parse_tool_entry("Write").unwrap();
        let input = serde_json::json!({"path": "/any/path"});
        assert!(m.matches("Write", Some(&input), None));
        assert!(m.matches("Write", None, None));
    }

    #[test]
    fn bare_name_rejects_wrong_tool() {
        let m = parse_tool_entry("Write").unwrap();
        assert!(!m.matches("Read", None, None));
    }

    #[test]
    fn glob_matches_path() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        let yes = serde_json::json!({"path": "/src/foo/bar.rs"});
        let no = serde_json::json!({"path": "/tests/foo.rs"});
        assert!(m.matches("Write", Some(&yes), None));
        assert!(!m.matches("Write", Some(&no), None));
    }

    #[test]
    fn glob_no_input_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        assert!(!m.matches("Write", None, None));
    }

    #[test]
    fn glob_missing_field_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        let input = serde_json::json!({"other_field": "/src/foo.rs"});
        assert!(!m.matches("Write", Some(&input), None));
    }

    #[test]
    fn regex_matches_path() {
        let m = parse_tool_entry(r"Write(regex:\.env)").unwrap();
        let yes = serde_json::json!({"path": "/home/user/.env"});
        let no = serde_json::json!({"path": "/src/main.rs"});
        assert!(m.matches("Write", Some(&yes), None));
        assert!(!m.matches("Write", Some(&no), None));
    }

    #[test]
    fn glob_matches_command_field() {
        let m = parse_tool_entry("Bash(npm *)").unwrap();
        let yes = serde_json::json!({"command": "npm test"});
        let no = serde_json::json!({"command": "cargo test"});
        assert!(m.matches("Bash", Some(&yes), None));
        assert!(!m.matches("Bash", Some(&no), None));
    }

    #[test]
    fn glob_matches_url_field() {
        let m = parse_tool_entry("WebFetch(https://example.com/**)").unwrap();
        let yes = serde_json::json!({"url": "https://example.com/page"});
        let no = serde_json::json!({"url": "https://other.com/page"});
        assert!(m.matches("WebFetch", Some(&yes), None));
        assert!(!m.matches("WebFetch", Some(&no), None));
    }

    // --- get_matchable_arg ---

    #[test]
    fn matchable_arg_file_tools() {
        let input = serde_json::json!({"path": "/foo/bar"});
        assert_eq!(
            get_matchable_arg("Read", &input, None),
            Some("/foo/bar".into())
        );
        assert_eq!(
            get_matchable_arg("Write", &input, None),
            Some("/foo/bar".into())
        );
        assert_eq!(
            get_matchable_arg("StrReplace", &input, None),
            Some("/foo/bar".into())
        );
        assert_eq!(
            get_matchable_arg("Delete", &input, None),
            Some("/foo/bar".into())
        );
    }

    #[test]
    fn matchable_arg_file_path_fallback() {
        let input = serde_json::json!({"file_path": "/foo/bar"});
        assert_eq!(
            get_matchable_arg("Edit", &input, None),
            Some("/foo/bar".into())
        );
        assert_eq!(
            get_matchable_arg("Read", &input, None),
            Some("/foo/bar".into())
        );
    }

    #[test]
    fn matchable_arg_shell() {
        let input = serde_json::json!({"command": "ls -la"});
        assert_eq!(
            get_matchable_arg("Bash", &input, None),
            Some("ls -la".into())
        );
        assert_eq!(
            get_matchable_arg("Shell", &input, None),
            Some("ls -la".into())
        );
    }

    #[test]
    fn matchable_arg_notebook() {
        let input = serde_json::json!({"target_notebook": "analysis.ipynb"});
        assert_eq!(
            get_matchable_arg("EditNotebook", &input, None),
            Some("analysis.ipynb".into())
        );
    }

    #[test]
    fn matchable_arg_unknown_tool() {
        let input = serde_json::json!({"whatever": "value"});
        assert_eq!(get_matchable_arg("UnknownTool", &input, None), None);
    }

    // --- cwd resolution ---

    #[test]
    fn cwd_resolves_relative_path() {
        let input = serde_json::json!({"path": "id_rsa"});
        assert_eq!(
            get_matchable_arg("Read", &input, Some("/home/user/.ssh")),
            Some("/home/user/.ssh/id_rsa".into())
        );
    }

    #[test]
    fn cwd_leaves_absolute_path_unchanged() {
        let input = serde_json::json!({"path": "/etc/passwd"});
        assert_eq!(
            get_matchable_arg("Read", &input, Some("/home/user")),
            Some("/etc/passwd".into())
        );
    }

    #[test]
    fn cwd_not_applied_to_shell_commands() {
        let input = serde_json::json!({"command": "cat id_rsa"});
        assert_eq!(
            get_matchable_arg("Bash", &input, Some("/home/user/.ssh")),
            Some("cat id_rsa".into())
        );
    }

    #[test]
    fn cwd_not_applied_to_urls() {
        let input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            get_matchable_arg("WebFetch", &input, Some("/home/user")),
            Some("https://example.com".into())
        );
    }

    #[test]
    fn cwd_resolves_dotdot_in_relative_path() {
        let input = serde_json::json!({"path": "../.ssh/id_rsa"});
        let resolved = get_matchable_arg("Read", &input, Some("/home/user/project")).unwrap();
        // Path::join preserves .. segments (no canonicalization), but the string
        // still contains .ssh so regex/glob patterns match
        assert!(resolved.contains(".ssh"));
    }

    // --- resolve_action (end-to-end) ---

    fn make_config(rules: Vec<Rule>, default: DefaultAction) -> ToolConfig {
        ToolConfig { default, rules }
    }

    fn make_rule(entries: &[&str], action: RuleAction) -> Rule {
        Rule {
            matchers: entries.iter().map(|e| parse_tool_entry(e).unwrap()).collect(),
            action,
            command: None,
            message: None,
        }
    }

    fn make_deny_rule(entries: &[&str], message: &str) -> Rule {
        Rule {
            matchers: entries.iter().map(|e| parse_tool_entry(e).unwrap()).collect(),
            action: RuleAction::Deny,
            command: None,
            message: Some(message.to_string()),
        }
    }

    #[test]
    fn deny_pattern_before_allow_bare() {
        let config = make_config(
            vec![
                make_rule(&["Write(**/.env*)"], RuleAction::Deny),
                make_rule(&["Write"], RuleAction::Allow),
            ],
            DefaultAction::Ask,
        );

        let env = serde_json::json!({"path": "/config/.env.local"});
        let normal = serde_json::json!({"path": "/src/main.rs"});

        assert!(matches!(
            resolve_action(&config, "Write", Some(&env), None),
            ResolvedAction::Deny(_)
        ));
        assert!(matches!(
            resolve_action(&config, "Write", Some(&normal), None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn unmatched_tool_falls_to_default() {
        let config = make_config(
            vec![make_rule(&["Write"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Read", None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn multiple_matchers_in_single_rule() {
        let config = make_config(
            vec![make_rule(&["Read", "Grep", "Glob"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Read", None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Grep", None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Write", None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn pattern_with_no_input_skips_rule() {
        let config = make_config(
            vec![
                make_rule(&["Write(/src/**)"], RuleAction::Allow),
                make_rule(&["Write"], RuleAction::Deny),
            ],
            DefaultAction::Ask,
        );
        // No tool_input: pattern rule can't match, falls through to bare "Write" -> Deny
        assert!(matches!(
            resolve_action(&config, "Write", None, None),
            ResolvedAction::Deny(_)
        ));
    }

    // --- absolute path enforcement ---

    #[test]
    fn relative_path_without_cwd_is_denied() {
        let config = make_config(
            vec![make_rule(&["Read"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "relative/path.txt"});
        match resolve_action(&config, "Read", Some(&input), None) {
            ResolvedAction::Deny(Some(msg)) => {
                assert!(msg.contains("absolute"), "expected absolute path error: {msg}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn relative_path_resolved_by_cwd_is_allowed() {
        let config = make_config(
            vec![make_rule(&["Read"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "src/main.rs"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), Some("/home/user/project")),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_shell() {
        let config = make_config(
            vec![make_rule(&["Bash"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"command": "ls relative/path"});
        assert!(matches!(
            resolve_action(&config, "Bash", Some(&input), None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_unknown_tools() {
        let config = make_config(vec![], DefaultAction::Allow);
        let input = serde_json::json!({"whatever": "relative/path"});
        assert!(matches!(
            resolve_action(&config, "UnknownTool", Some(&input), None),
            ResolvedAction::Allow
        ));
    }

    // --- deny messages ---

    #[test]
    fn deny_rule_with_message() {
        let config = make_config(
            vec![make_deny_rule(
                &["Delete"],
                "Use trash instead of delete",
            )],
            DefaultAction::Ask,
        );
        match resolve_action(&config, "Delete", None, None) {
            ResolvedAction::Deny(Some(msg)) => {
                assert_eq!(msg, "Use trash instead of delete");
            }
            other => panic!("expected Deny with message, got {other:?}"),
        }
    }

    #[test]
    fn deny_rule_without_message() {
        let config = make_config(
            vec![make_rule(&["Delete"], RuleAction::Deny)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Delete", None, None),
            ResolvedAction::Deny(None)
        ));
    }

    #[test]
    fn deny_default_has_no_message() {
        let config = make_config(vec![], DefaultAction::Deny);
        assert!(matches!(
            resolve_action(&config, "Write", None, None),
            ResolvedAction::Deny(None)
        ));
    }

    // --- cwd + pattern matching integration ---

    #[test]
    fn cwd_resolved_path_matches_pattern() {
        let config = make_config(
            vec![
                make_rule(&[r"Read(regex:\.ssh)"], RuleAction::Deny),
                make_rule(&["Read"], RuleAction::Allow),
            ],
            DefaultAction::Ask,
        );
        // Relative path "id_rsa" with cwd ~/.ssh -> resolved to /home/user/.ssh/id_rsa
        let input = serde_json::json!({"path": "id_rsa"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), Some("/home/user/.ssh")),
            ResolvedAction::Deny(_)
        ));
    }
}

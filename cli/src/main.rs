mod attended;
mod attended_relay;
mod build;
mod client;
mod git_view;
mod host_run;
mod local;
mod review;
mod state;
mod validate;
mod workflows;

#[cfg(test)]
mod attended_state_tests;
#[cfg(test)]
mod local_tests;

use std::io::Read as _;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
use crate::local::{RequestedContextRoot, RequestedWorkspace, WorkspaceAccess};
use crate::state::StateStore;
use anyhow::Context as _;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-hub", version, about = "Agent Hub CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run locally authorized agents
    #[command(
        after_help = "Examples:\n  agent-hub local run --request-stdin < agent-hub-run.json\n  agent-hub local inspect --request-stdin < agent-hub-run.json"
    )]
    Local {
        #[command(subcommand)]
        command: Box<LocalCommand>,
    },
    /// Launch the interactive approval review TUI
    Review {
        /// Server URL
        #[arg(
            long,
            env = "AGENT_HUB_SERVER",
            default_value = "http://localhost:8080"
        )]
        server: String,

        /// Bearer token for authenticated servers (optional for no-auth mode)
        #[arg(long, env = "AGENT_HUB_TOKEN")]
        token: Option<String>,

        /// Poll interval in seconds
        #[arg(long, default_value = "2")]
        poll_interval: u64,
    },
    /// Validate gateway config, delegate commands, and server connectivity
    Validate {
        /// Path to tool routing config file (JSON)
        #[arg(long, default_value = "~/.config/agent-hub/agent-hub-permissions.json")]
        config: String,

        /// Server URL (optional; enables server connectivity check)
        #[arg(long, env = "AGENT_HUB_SERVER")]
        server: Option<String>,

        /// Bearer token (optional; enables auth check when --server is provided)
        #[arg(long, env = "AGENT_HUB_TOKEN")]
        token: Option<String>,
    },
    /// Validate orchestration workflow TOML definitions
    ValidateWorkflows {
        /// Directory containing workflow .toml files
        #[arg(long, default_value = "workflows")]
        dir: String,
    },
    /// Build Docker images and materialize environment artifacts
    Build {
        /// Path to config root (agent-hub-configs directory)
        #[arg(long, env = "CONFIG_ROOT", default_value = "../agent-hub-configs")]
        config_root: String,

        /// Output directory for materialized artifacts
        #[arg(long)]
        output_dir: Option<String>,

        /// What to build
        #[command(subcommand)]
        target: Option<BuildTarget>,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum BuildTarget {
    /// Build Docker images only
    Images,
    /// Materialize environment artifacts only
    Environments,
}

#[derive(Subcommand)]
enum LocalCommand {
    /// Host-owned one-step prepare, approve, and execute path (structured JSON on stdin)
    #[command(name = "run", alias = "agent-hub-run")]
    AgentHubRun {
        #[arg(long, action = clap::ArgAction::SetTrue, required = true)]
        request_stdin: bool,
        /// Explicit host config; automatic rules require the trusted default identity
        #[arg(long)]
        host_config: Option<PathBuf>,
        #[arg(
            long,
            env = "AGENT_HUB_SERVER",
            default_value = "http://localhost:8080"
        )]
        approvals_server: String,
        #[arg(long, env = "AGENT_HUB_TOKEN")]
        approvals_token: Option<String>,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=600))]
        approval_timeout_secs: u64,
        /// Stable internal invocation UUID for manual resume/reconciliation
        #[arg(long)]
        internal_invocation_id: Option<String>,
    },
    /// Debug canonical preparation without requesting permission or executing
    Inspect {
        #[arg(long, action = clap::ArgAction::SetTrue, required = true)]
        request_stdin: bool,
        #[arg(long)]
        host_config: Option<PathBuf>,
    },
    /// Preview trusted projection and matched rule without side effects
    Preview {
        #[arg(long, action = clap::ArgAction::SetTrue, required = true)]
        request_stdin: bool,
        #[arg(long)]
        host_config: Option<PathBuf>,
    },
    /// List local attended executions
    List,
    /// Show one execution record
    Status {
        /// Canonical run UUID printed by `local run` or `local list`
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
    },
    /// Print an execution log
    Logs {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        #[arg(long, value_parser = ["stdout", "stderr", "transcript"], default_value = "transcript")]
        stream: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Print an execution result
    Result {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Print the bounded artifact manifest
    Artifacts {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Print the bounded repository result document
    Repositories {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Print the bounded cleanup result document
    CleanupResult {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Cancel an owned running execution
    Cancel {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
    },
    /// Explicitly remove owned execution resources and artifacts
    Cleanup {
        #[arg(long, value_parser = validate_run_id)]
        run_id: String,
        /// Host-only approvals endpoint; never passed to the sandbox
        #[arg(
            long,
            env = "AGENT_HUB_SERVER",
            default_value = "http://localhost:8080"
        )]
        approvals_server: String,
        /// Host-only bearer credential; never passed to the sandbox
        #[arg(long, env = "AGENT_HUB_TOKEN")]
        approvals_token: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.command {
        Commands::Local { command } => rt.block_on(run_local(*command)),
        Commands::Review {
            server,
            token,
            poll_interval,
        } => rt.block_on(review::run(server, token, poll_interval)),
        Commands::Validate {
            config,
            server,
            token,
        } => {
            let code = rt.block_on(validate::run(config, server, token));
            std::process::exit(code);
        }
        Commands::ValidateWorkflows { dir } => {
            let code = workflows::run(dir);
            std::process::exit(code);
        }
        Commands::Build {
            config_root,
            output_dir,
            target,
        } => {
            let code = build::run(config_root, output_dir, target);
            std::process::exit(code);
        }
    }
}

async fn run_local(command: LocalCommand) -> anyhow::Result<()> {
    match command {
        LocalCommand::AgentHubRun {
            request_stdin: _,
            host_config,
            approvals_server,
            approvals_token,
            approval_timeout_secs,
            internal_invocation_id,
        } => {
            let fallback_invocation_id = uuid::Uuid::new_v4().to_string();
            let invocation_id = match host_invocation_id(internal_invocation_id.as_deref()) {
                Ok(invocation_id) => invocation_id,
                Err(error) => {
                    return emit_host_run_output(host_run_failure_output(
                        fallback_invocation_id,
                        &error,
                    ));
                }
            };
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let run = async {
                let intent = read_host_run_request()?;
                let trusted_host_config = host_run::default_host_config()?;
                let store = state_store()?;
                host_run::run(
                    &store,
                    intent,
                    host_run::HostRunOptions {
                        host_config: host_config.unwrap_or_else(|| trusted_host_config.clone()),
                        trusted_host_config,
                        approvals_server,
                        approvals_token: approvals_token.map(protocol::Secret::new),
                        approval_timeout_secs,
                        invocation_id: invocation_id.clone(),
                        cancellation: cancel_rx,
                    },
                )
                .await
            };
            tokio::pin!(run);
            #[cfg(unix)]
            let outcome = tokio::select! {
                outcome = &mut run => outcome,
                signal = one_step_shutdown_signal() => {
                    signal?;
                    let _ = cancel_tx.send(true);
                    tokio::time::timeout(
                        std::time::Duration::from_secs(360),
                        &mut run,
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!(
                        "agent_hub_run cancellation exceeded its terminal deadline"
                    )))
                }
            };
            #[cfg(not(unix))]
            let outcome = run.await;
            let output = match outcome {
                Ok(output) => output,
                Err(error) => host_run_failure_output(invocation_id.clone(), &error),
            };
            emit_host_run_output(output)?;
        }
        LocalCommand::Inspect {
            request_stdin: _,
            host_config,
        } => {
            let config = match host_config {
                Some(path) => path,
                None => host_run::default_host_config()?,
            };
            let artifact = host_run::inspect(read_host_run_request()?, &config).await?;
            println!("{}", serde_json::to_string_pretty(&artifact)?);
        }
        LocalCommand::Preview {
            request_stdin: _,
            host_config,
        } => {
            let config = match host_config {
                Some(path) => path,
                None => host_run::default_host_config()?,
            };
            let preview = host_run::preview(read_host_run_request()?, &config).await?;
            println!("{}", serde_json::to_string_pretty(&preview)?);
        }
        LocalCommand::List => {
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::list(&state_store()?)?)?
            );
        }
        LocalCommand::Status { run_id } => {
            let record = attended::status(&state_store()?, &run_id)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_successful_outcome(&record)?;
        }
        LocalCommand::Logs {
            run_id,
            stream,
            limit_bytes,
        } => {
            let name = match stream.as_str() {
                "stdout" => "stdout.log",
                "stderr" => "stderr.log",
                "transcript" => "transcript.jsonl",
                _ => unreachable!("clap validates log stream"),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::read_artifact_bounded(
                    &state_store()?,
                    &run_id,
                    name,
                    limit_bytes
                )?)?
            );
        }
        LocalCommand::Result {
            run_id,
            limit_bytes,
        } => {
            let store = state_store()?;
            let record = attended::status(&store, &run_id)?;
            let result = attended::local_result(&store, &run_id, limit_bytes)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            attended::ensure_local_result_documents(&result)?;
            attended::ensure_successful_outcome(&record)?;
        }
        LocalCommand::Artifacts {
            run_id,
            limit_bytes,
        } => {
            let store = state_store()?;
            let document =
                attended::read_document_bounded(&store, &run_id, "artifacts.json", limit_bytes)?;
            println!("{}", serde_json::to_string_pretty(&document)?);
            attended::ensure_document_complete(&document)?;
        }
        LocalCommand::Repositories {
            run_id,
            limit_bytes,
        } => {
            let store = state_store()?;
            let document =
                attended::read_document_bounded(&store, &run_id, "repositories.json", limit_bytes)?;
            println!("{}", serde_json::to_string_pretty(&document)?);
            attended::ensure_document_complete(&document)?;
        }
        LocalCommand::CleanupResult {
            run_id,
            limit_bytes,
        } => {
            let store = state_store()?;
            let record = attended::status(&store, &run_id)?;
            let document =
                attended::read_document_bounded(&store, &run_id, "cleanup.json", limit_bytes)?;
            println!("{}", serde_json::to_string_pretty(&document)?);
            attended::ensure_document_complete(&document)?;
            attended::ensure_cleanup_successful(&record)?;
        }
        LocalCommand::Cancel { run_id } => {
            attended::cancel(&state_store()?, &run_id).await?;
            let record = attended::status(&state_store()?, &run_id)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_successful_outcome(&record)?;
        }
        LocalCommand::Cleanup {
            run_id,
            approvals_server,
            approvals_token,
        } => {
            let record = attended::cleanup(
                &state_store()?,
                &run_id,
                attended::RelayEndpoint {
                    server_url: approvals_server,
                    token: approvals_token.map(protocol::Secret::new),
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_cleanup_successful(&record)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn one_step_shutdown_signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

fn host_invocation_id(explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(explicit) = explicit {
        let id = uuid::Uuid::parse_str(explicit).context("internal invocation ID is not a UUID")?;
        anyhow::ensure!(
            id.to_string() == explicit,
            "internal invocation ID is not canonical"
        );
        return Ok(explicit.to_string());
    }
    let session = std::env::var("AGENT_HUB_TRUSTED_SESSION_ID").ok();
    let call = std::env::var("AGENT_HUB_TRUSTED_TOOL_CALL_ID").ok();
    match (session, call) {
        (Some(session), Some(call)) if !session.is_empty() && !call.is_empty() => {
            Ok(trusted_host_invocation_id(&session, &call))
        }
        (None, None) => Ok(uuid::Uuid::new_v4().to_string()),
        _ => anyhow::bail!(
            "trusted one-step invocation requires both session and tool-call identities"
        ),
    }
}

fn trusted_host_invocation_id(session: &str, call: &str) -> String {
    let digest = protocol::sha256_hex_with_domain(
        b"agent-hub:trusted-one-step-invocation:v1\0",
        format!("{session}\0{call}").as_bytes(),
    );
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16)
            .expect("SHA-256 digest is lowercase hexadecimal");
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn bounded_error(error: &anyhow::Error) -> String {
    bounded_text(error.to_string())
}

fn bounded_text(mut message: String) -> String {
    if message.len() > protocol::AGENT_HUB_RUN_MAX_REASON_BYTES {
        let mut end = protocol::AGENT_HUB_RUN_MAX_REASON_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

fn host_run_failure_output(
    invocation_id: String,
    error: &anyhow::Error,
) -> protocol::AgentHubRunOutputV1 {
    let message = bounded_error(error);
    protocol::AgentHubRunOutputV1 {
        schema: protocol::AGENT_HUB_RUN_OUTPUT_SCHEMA_V1.into(),
        invocation_id,
        approval_receipt: protocol::AgentHubRunApprovalReceiptV1::unavailable(message.clone()),
        local_result_v2: serde_json::json!({
            "schema": "local_result_v2",
            "run_id": null,
            "status": "failed",
            "cleanup_outcome": "unknown"
        }),
        error: Some(message),
    }
}

fn emit_host_run_output(mut output: protocol::AgentHubRunOutputV1) -> anyhow::Result<()> {
    if let Err(error) = output.validate() {
        output = host_run_failure_output(
            output.invocation_id.clone(),
            &anyhow::anyhow!("invalid agent_hub_run terminal output: {error}"),
        );
        output.validate().map_err(anyhow::Error::msg)?;
    }
    println!("{}", serde_json::to_string(&output)?);
    if let Some(error) = output.error.as_deref() {
        anyhow::bail!("{error}");
    }
    let receipt = output
        .approval_receipt
        .receipt
        .as_ref()
        .context("one-step output has no issued approval receipt")?;
    anyhow::ensure!(
        receipt.decision() == protocol::AgentHubRunDecision::Allow,
        "agent_hub_run was denied: {}",
        receipt.reason().unwrap_or("denied by policy")
    );
    anyhow::ensure!(
        output.local_result_v2["execution_outcome"] == "succeeded"
            && output.local_result_v2["result_outcome"] == "succeeded"
            && output.local_result_v2["cleanup_outcome"] == "succeeded",
        "agent_hub_run did not complete successfully"
    );
    Ok(())
}

fn read_host_run_request() -> anyhow::Result<protocol::AgentHubRunIntentV1> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(protocol::ATTENDED_GATEWAY_MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= protocol::ATTENDED_GATEWAY_MAX_FRAME_BYTES,
        "agent_hub_run request exceeds limit"
    );
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn state_store() -> anyhow::Result<StateStore> {
    let root = match std::env::var_os("AGENT_HUB_ATTENDED_STATE") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is required"))?,
        )
        .join(".local/state/agent-hub/attended"),
    };
    StateStore::open(root)
}

#[cfg(test)]
fn parse_workspace(value: &str) -> Result<RequestedWorkspace, String> {
    let (alias, raw) = value
        .split_once('=')
        .ok_or_else(|| "workspace must be ALIAS=CANONICAL_ABSOLUTE_PATH:rw|ro".to_string())?;
    if alias.is_empty() || raw.is_empty() || raw.contains('=') {
        return Err("workspace must have one non-empty alias and path".to_string());
    }
    validate_cli_alias(alias)?;
    let (path, access) = if let Some(path) = raw.strip_suffix(":ro") {
        (path, WorkspaceAccess::ReadOnly)
    } else if let Some(path) = raw.strip_suffix(":rw") {
        (path, WorkspaceAccess::ReadWrite)
    } else {
        return Err("workspace access mode must be explicit :rw or :ro".to_string());
    };
    validate_cli_mount_path(path)?;
    Ok(RequestedWorkspace {
        alias: alias.to_string(),
        path: PathBuf::from(path),
        access,
    })
}

#[cfg(test)]
fn parse_context_root(value: &str) -> Result<RequestedContextRoot, String> {
    let (alias, raw) = value
        .split_once('=')
        .ok_or_else(|| "context root must be ALIAS=CANONICAL_ABSOLUTE_PATH:ro".to_string())?;
    if alias.is_empty() || raw.is_empty() || raw.contains('=') {
        return Err("context root must have one non-empty alias and path".to_string());
    }
    validate_cli_alias(alias)?;
    let path = raw
        .strip_suffix(":ro")
        .ok_or_else(|| "context root access mode must be explicit :ro".to_string())?;
    validate_cli_mount_path(path)?;
    Ok(RequestedContextRoot {
        alias: alias.to_string(),
        path: PathBuf::from(path),
        access: WorkspaceAccess::ReadOnly,
    })
}

#[cfg(test)]
fn validate_cli_alias(alias: &str) -> Result<String, String> {
    if alias.is_empty() || alias.len() > 63 {
        return Err("mount alias must contain 1 to 63 bytes".into());
    }
    if !alias
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !alias.as_bytes()[0].is_ascii_alphanumeric()
        || !alias.as_bytes()[alias.len() - 1].is_ascii_alphanumeric()
    {
        return Err(
            "mount alias must use lowercase ASCII letters or digits with hyphens only between alphanumeric characters"
                .into(),
        );
    }
    if ["artifacts", "config", "context", "opencode", "output"].contains(&alias) {
        return Err(format!(
            "mount alias '{alias}' is reserved; choose a different alias"
        ));
    }
    Ok(alias.to_string())
}

fn validate_run_id(value: &str) -> Result<String, String> {
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| {
        "run ID must be a canonical hyphenated UUID printed by `agent-hub local run` or `agent-hub local list`"
            .to_string()
    })?;
    if parsed.hyphenated().to_string() != value {
        return Err(
            "run ID must use canonical lowercase hyphenated UUID form (for example 123e4567-e89b-12d3-a456-426614174000)"
                .to_string(),
        );
    }
    Ok(value.to_string())
}

#[cfg(test)]
fn validate_cli_mount_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || !Path::new(path).is_absolute()
        || path.contains(':')
        || path.contains(',')
        || path.chars().any(char::is_control)
    {
        return Err(
            "mount path must be a non-empty canonical absolute path without delimiters".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod attended_cli_tests {
    use super::*;

    #[test]
    fn mount_grammar_requires_explicit_modes_and_absolute_paths() {
        assert!(parse_workspace("repo=/tmp/repo:rw").is_ok());
        assert!(parse_workspace("repo=/tmp/repo:ro").is_ok());
        assert!(parse_workspace("repo=/tmp/repo").is_err());
        assert!(parse_workspace("repo=relative:rw").is_err());
        assert!(parse_workspace("=/tmp/repo:rw").is_err());
        assert!(parse_context_root("code=/home/nick.davies/code:ro").is_ok());
        assert!(parse_context_root("code=/home/nick.davies/code").is_err());
        assert!(parse_context_root("code=/home/nick.davies/code:rw").is_err());
    }

    #[test]
    fn legacy_commands_and_bypass_flags_are_rejected() {
        assert!(Cli::try_parse_from(["agent-hub", "local", "propose", "--help"]).is_err());
        assert!(Cli::try_parse_from(["agent-hub", "local", "authorize", "--help"]).is_err());
        for flag in [
            "--authorization",
            "--digest",
            "--allow",
            "--bypass",
            "--skip-approval",
        ] {
            assert!(
                Cli::try_parse_from(["agent-hub", "local", "run", "--request-stdin", flag, "x"])
                    .is_err()
            );
        }
    }

    /*
    fn obsolete_proposal_contract() {
        let valid = vec![
            "agent-hub",
            "local",
            "propose",
            "--config-root",
            "/config",
            "--environment",
            "development",
            "--image-family",
            "job-code",
            "--network-class",
            "none",
            "--tool-approval",
            "auto",
            "--primary-workspace",
            "primary",
            "--workspace",
            "primary=/tmp/repo:rw",
            "--prompt-stdin",
            "--timeout-secs",
            "3600",
            "--output",
            "authorization.json",
        ];
        assert!(Cli::try_parse_from(valid.clone()).is_ok());
        for required in [
            "--environment",
            "--image-family",
            "--network-class",
            "--tool-approval",
            "--primary-workspace",
            "--workspace",
            "--timeout-secs",
        ] {
            let mut omitted = valid.clone();
            let index = omitted.iter().position(|value| *value == required).unwrap();
            omitted.drain(index..=index + 1);
            assert!(
                Cli::try_parse_from(omitted).is_err(),
                "accepted proposal without {required}"
            );
        }
        let mut omitted_stdin = valid;
        omitted_stdin.retain(|value| *value != "--prompt-stdin");
        assert!(Cli::try_parse_from(omitted_stdin).is_err());
        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "local",
                "propose",
                "--config-root",
                "/config",
                "--environment",
                "development",
                "--primary-workspace",
                "primary",
                "--workspace",
                "primary=/tmp/repo:rw",
                "--prompt",
                "text",
                "--output",
                "authorization.json"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "local",
                "propose",
                "--config-root",
                "/config",
                "--environment",
                "development",
                "--primary-workspace",
                "primary",
                "--workspace",
                "primary=/tmp/repo:rw",
                "--prompt-file",
                "/tmp/prompt",
                "--output",
                "authorization.json"
            ])
            .is_err()
        );
    }

    fn obsolete_run_contract() {
        let valid = vec![
            "agent-hub",
            "local",
            "run",
            "--environment",
            "development",
            "--image-family",
            "job-code",
            "--network-class",
            "build",
            "--network-resource",
            "llm",
            "--tool-approval",
            "dippy",
            "--overlay",
            "github",
            "--timeout-secs",
            "3600",
            "--authorization",
            "/tmp/authorization.json",
            "--digest",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        assert!(Cli::try_parse_from(valid.clone()).is_ok());

        let mut gateway = valid.clone();
        gateway[12] = "gateway";
        assert!(Cli::try_parse_from(gateway).is_err());

        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "local",
                "run",
                "--authorization",
                "/tmp/auth.json",
                "--environment",
                "development",
                "--image-family",
                "job-code",
                "--network-class",
                "build",
                "--network-resource",
                "llm",
                "--tool-approval",
                "dippy",
                "--overlay",
                "github",
                "--timeout-secs",
                "3600",
                "--digest",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["agent-hub", "local", "authorize", "--help"]).is_err());
    }
    */

    #[test]
    fn result_document_commands_have_stable_cli_shapes() {
        let run_id = "123e4567-e89b-12d3-a456-426614174000";
        for command in ["result", "artifacts", "repositories", "cleanup-result"] {
            let parsed = Cli::try_parse_from([
                "agent-hub",
                "local",
                command,
                "--run-id",
                run_id,
                "--limit-bytes",
                "4096",
            ]);
            assert!(parsed.is_ok(), "failed to parse local {command}");
        }

        for invalid in ["not-a-uuid", "123e4567e89b12d3a456426614174000"] {
            assert!(
                Cli::try_parse_from(["agent-hub", "local", "artifacts", "--run-id", invalid,])
                    .is_err()
            );
        }
    }

    #[test]
    fn trusted_host_invocation_ids_are_canonical_deterministic_v8_uuids() {
        let first = trusted_host_invocation_id("session", "call");
        assert_eq!(first, "22356e2a-8bf3-841c-a45a-4aaf1ab6c463");
        assert_eq!(first, trusted_host_invocation_id("session", "call"));
        assert_ne!(first, trusted_host_invocation_id("session", "other-call"));
        assert_ne!(first, trusted_host_invocation_id("other-session", "call"));

        let parsed = uuid::Uuid::parse_str(&first).unwrap();
        assert_eq!(parsed.to_string(), first);
        assert_eq!(first.as_bytes()[14], b'8');
        assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn explicit_host_invocation_ids_remain_strictly_canonical() {
        let canonical = "123e4567-e89b-42d3-a456-426614174000";
        assert_eq!(host_invocation_id(Some(canonical)).unwrap(), canonical);
        assert!(host_invocation_id(Some("123E4567-E89B-42D3-A456-426614174000")).is_err());
        assert!(host_invocation_id(Some("123e4567e89b42d3a456426614174000")).is_err());
    }
}

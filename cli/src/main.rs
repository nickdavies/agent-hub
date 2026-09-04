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
    },
    /// Debug canonical preparation without requesting permission or executing
    Inspect {
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
        } => {
            let intent = read_host_run_request()?;
            let trusted_host_config = host_run::default_host_config()?;
            let output = host_run::run(
                &state_store()?,
                intent,
                host_run::HostRunOptions {
                    host_config: host_config.unwrap_or_else(|| trusted_host_config.clone()),
                    trusted_host_config,
                    approvals_server,
                    approvals_token: approvals_token.map(protocol::Secret::new),
                    approval_timeout_secs,
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&output)?);
            anyhow::ensure!(
                output.receipt.decision() == protocol::AgentHubRunDecision::Allow,
                "agent_hub_run was denied: {}",
                output.receipt.reason().unwrap_or("denied by policy")
            );
            attended::ensure_successful_outcome(output.run.as_ref().expect("allow has run"))?;
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
}

mod build;
mod client;
mod review;
mod validate;
mod workflows;

use std::io::Read as _;
use std::path::{Path, PathBuf};

use agent_hub::attended::{self, AuthorizationSelectors, ProposalInput};
use agent_hub::local::{RequestedContextRoot, RequestedWorkspace, WorkspaceAccess};
use agent_hub::state::StateStore;
use clap::{Parser, Subcommand};
use protocol::NetworkResourceKind;

#[derive(Parser)]
#[command(name = "agent-hub", version, about = "Agent Hub CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run locally authorized attended agents
    Attended {
        #[command(subcommand)]
        command: AttendedCommand,
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
enum AttendedCommand {
    /// Create a deterministic attended authorization proposal
    Propose {
        #[arg(long, env = "CONFIG_ROOT")]
        config_root: PathBuf,
        #[arg(long)]
        environment: String,
        #[arg(long = "network-resource")]
        network_resources: Vec<NetworkResourceKind>,
        #[arg(long = "workspace", value_parser = parse_workspace, required = true)]
        workspaces: Vec<RequestedWorkspace>,
        #[arg(long = "context-root", value_parser = parse_context_root)]
        context_roots: Vec<RequestedContextRoot>,
        #[arg(long, value_parser = validate_cli_alias)]
        primary_workspace: String,
        /// Read the complete prompt from stdin
        #[arg(long, action = clap::ArgAction::SetTrue, required = true)]
        prompt_stdin: bool,
        #[arg(long)]
        output: PathBuf,
    },
    /// Record a selector-bound Dippy authorization
    Authorize {
        #[arg(long)]
        environment: String,
        #[arg(long, value_parser = ["job-code"])]
        image_family: String,
        #[arg(long, value_parser = ["none", "llm-only", "build", "guarded-web"])]
        network_class: String,
        #[arg(long = "network-resource")]
        network_resources: Vec<NetworkResourceKind>,
        #[arg(long, value_parser = ["auto", "dippy"])]
        tool_approval: String,
        #[arg(long)]
        authorization: PathBuf,
        #[arg(long)]
        digest: String,
    },
    /// Consume an authorization and execute it in the foreground
    Run {
        #[arg(long)]
        authorization: PathBuf,
    },
    /// List local attended executions
    List,
    /// Show one execution record
    Status { id: String },
    /// Print an execution log
    Logs {
        id: String,
        #[arg(long, value_parser = ["stdout", "stderr", "transcript"], default_value = "transcript")]
        stream: String,
    },
    /// Print an execution result
    Result { id: String },
    /// Cancel an owned running execution
    Cancel { id: String },
    /// Explicitly remove owned execution resources and artifacts
    Cleanup { id: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.command {
        Commands::Attended { command } => rt.block_on(run_attended(command)),
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

async fn run_attended(command: AttendedCommand) -> anyhow::Result<()> {
    match command {
        AttendedCommand::Propose {
            config_root,
            environment,
            network_resources,
            workspaces,
            context_roots,
            primary_workspace,
            prompt_stdin: _,
            output,
        } => {
            let mut prompt = String::new();
            std::io::stdin().read_to_string(&mut prompt)?;
            let artifact = attended::build_proposal(ProposalInput {
                config_root,
                environment,
                network_resources,
                prompt,
                workspaces,
                context_roots,
                primary_workspace,
            })?;
            write_new_private_json(&output, &artifact)?;
            print!("{}", attended::render_proposal(&output, &artifact)?);
        }
        AttendedCommand::Authorize {
            environment,
            image_family,
            network_class,
            network_resources,
            tool_approval,
            authorization,
            digest,
        } => {
            attended::authorize(
                &state_store()?,
                &authorization,
                AuthorizationSelectors {
                    digest: &digest,
                    environment: &environment,
                    image_family: &image_family,
                    network_class: &network_class,
                    network_resources: &network_resources,
                    tool_approval: &tool_approval,
                },
            )?;
        }
        AttendedCommand::Run { authorization } => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &attended::run(&state_store()?, &authorization).await?
                )?
            );
        }
        AttendedCommand::List => {
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::list(&state_store()?)?)?
            );
        }
        AttendedCommand::Status { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::status(&state_store()?, &id)?)?
            );
        }
        AttendedCommand::Logs { id, stream } => {
            let name = match stream.as_str() {
                "stdout" => "stdout.log",
                "stderr" => "stderr.log",
                "transcript" => "transcript.jsonl",
                _ => unreachable!("clap validates log stream"),
            };
            print!(
                "{}",
                std::fs::read_to_string(attended::artifact_path(&state_store()?, &id, name)?)?
            );
        }
        AttendedCommand::Result { id } => {
            print!(
                "{}",
                std::fs::read_to_string(attended::artifact_path(
                    &state_store()?,
                    &id,
                    "result.json"
                )?)?
            );
        }
        AttendedCommand::Cancel { id } => attended::cancel(&state_store()?, &id).await?,
        AttendedCommand::Cleanup { id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::cleanup(&state_store()?, &id).await?)?
            );
        }
    }
    Ok(())
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

fn validate_cli_alias(alias: &str) -> Result<String, String> {
    if alias.is_empty()
        || alias.len() > 63
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !alias.as_bytes()[0].is_ascii_alphanumeric()
        || !alias.as_bytes()[alias.len() - 1].is_ascii_alphanumeric()
        || ["artifacts", "config", "context", "opencode", "output"].contains(&alias)
    {
        return Err("mount alias is not canonical or is reserved".into());
    }
    Ok(alias.to_string())
}

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

fn write_new_private_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write as _;
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "authorization output path is empty"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("authorization output has no parent"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    anyhow::ensure!(
        parent.is_dir(),
        "authorization output parent does not exist"
    );
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    std::fs::File::open(parent)?.sync_all()?;
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
    fn proposal_prompt_contract_has_only_explicit_stdin() {
        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "attended",
                "propose",
                "--config-root",
                "/config",
                "--environment",
                "development",
                "--primary-workspace",
                "primary",
                "--workspace",
                "primary=/tmp/repo:rw",
                "--prompt-stdin",
                "--output",
                "authorization.json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "attended",
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
                "attended",
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

    #[test]
    fn authorization_contract_rejects_internal_names_and_alternate_options() {
        let valid = [
            "agent-hub",
            "attended",
            "authorize",
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
            "--authorization",
            "/tmp/authorization.json",
            "--digest",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        assert!(Cli::try_parse_from(valid).is_ok());

        let mut gateway = valid;
        gateway[12] = "gateway";
        assert!(Cli::try_parse_from(gateway).is_err());

        let mut resource = valid;
        resource[9] = "--resource";
        assert!(Cli::try_parse_from(resource).is_err());

        assert!(
            Cli::try_parse_from([
                "agent-hub",
                "attended",
                "run",
                "--authorization",
                "/tmp/auth.json",
                "--environment",
                "development"
            ])
            .is_err()
        );
    }
}

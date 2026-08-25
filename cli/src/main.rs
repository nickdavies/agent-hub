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
    /// Run locally authorized agents
    #[command(
        after_help = "Examples:\n  agent-hub local propose --config-root /config --environment code-supervised --image-family job-code --network-class build --network-resource llm --tool-approval dippy --overlay github --workspace primary=/workspace/repo:rw --primary-workspace primary --context-root code=/home/nick.davies/code:ro --timeout-secs 3600 --prompt-stdin --output /secure/authorization.json < /secure/prompt\n  agent-hub local run --environment code-supervised --image-family job-code --network-class build --tool-approval dippy --authorization /secure/authorization.json --digest DIGEST"
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
    /// Create a deterministic attended authorization proposal
    Propose {
        #[arg(long, env = "CONFIG_ROOT")]
        config_root: PathBuf,
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
        #[arg(long = "overlay")]
        overlays: Vec<String>,
        #[arg(long = "workspace", value_parser = parse_workspace, required = true)]
        workspaces: Vec<RequestedWorkspace>,
        #[arg(long = "context-root", value_parser = parse_context_root)]
        context_roots: Vec<RequestedContextRoot>,
        #[arg(long, value_parser = validate_cli_alias)]
        primary_workspace: String,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        timeout_secs: u64,
        /// Read proposal transport from stdin; the prompt is persisted in the private authorization
        #[arg(long, action = clap::ArgAction::SetTrue, required = true)]
        prompt_stdin: bool,
        #[arg(long)]
        output: PathBuf,
    },
    /// Consume an authorization and execute it in the foreground
    Run {
        #[arg(long)]
        environment: String,
        #[arg(long, value_parser = ["job-code"])]
        image_family: String,
        #[arg(long, value_parser = ["none", "llm-only", "build", "guarded-web"])]
        network_class: String,
        #[arg(long, value_parser = ["auto", "dippy"])]
        tool_approval: String,
        #[arg(long)]
        authorization: PathBuf,
        #[arg(long)]
        digest: String,
    },
    /// List local attended executions
    List,
    /// Show one execution record
    Status {
        #[arg(long)]
        run_id: String,
    },
    /// Print an execution log
    Logs {
        #[arg(long)]
        run_id: String,
        #[arg(long, value_parser = ["stdout", "stderr", "transcript"], default_value = "transcript")]
        stream: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Print an execution result
    Result {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 1_048_576, value_parser = clap::value_parser!(u64).range(1..=16_777_216))]
        limit_bytes: u64,
    },
    /// Cancel an owned running execution
    Cancel {
        #[arg(long)]
        run_id: String,
    },
    /// Explicitly remove owned execution resources and artifacts
    Cleanup {
        #[arg(long)]
        run_id: String,
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
        LocalCommand::Propose {
            config_root,
            environment,
            image_family,
            network_class,
            network_resources,
            tool_approval,
            overlays,
            workspaces,
            context_roots,
            primary_workspace,
            timeout_secs,
            prompt_stdin: _,
            output,
        } => {
            let mut prompt = String::new();
            std::io::stdin().read_to_string(&mut prompt)?;
            let artifact = attended::build_proposal(ProposalInput {
                config_root,
                environment,
                image_family,
                network_class,
                network_resources,
                tool_approval,
                overlays,
                timeout_secs,
                prompt,
                workspaces,
                context_roots,
                primary_workspace,
            })
            .await?;
            write_new_private_json(&output, &artifact)?;
            print!("{}", attended::render_proposal(&output, &artifact)?);
        }
        LocalCommand::Run {
            environment,
            image_family,
            network_class,
            tool_approval,
            authorization,
            digest,
        } => {
            let record = attended::run(
                &state_store()?,
                &authorization,
                AuthorizationSelectors {
                    digest: &digest,
                    environment: &environment,
                    image_family: &image_family,
                    network_class: &network_class,
                    tool_approval: &tool_approval,
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_successful_outcome(&record)?;
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
            let record = attended::status(&state_store()?, &run_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&attended::read_artifact_bounded(
                    &state_store()?,
                    &run_id,
                    "result.json",
                    limit_bytes
                )?)?
            );
            attended::ensure_successful_outcome(&record)?;
        }
        LocalCommand::Cancel { run_id } => {
            attended::cancel(&state_store()?, &run_id).await?;
            let record = attended::status(&state_store()?, &run_id)?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_successful_outcome(&record)?;
        }
        LocalCommand::Cleanup { run_id } => {
            let record = attended::cleanup(&state_store()?, &run_id).await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            attended::ensure_successful_outcome(&record)?;
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

    #[test]
    fn run_contract_has_exact_ordered_selectors_authorization_and_digest() {
        let valid = [
            "agent-hub",
            "local",
            "run",
            "--environment",
            "development",
            "--image-family",
            "job-code",
            "--network-class",
            "build",
            "--tool-approval",
            "dippy",
            "--authorization",
            "/tmp/authorization.json",
            "--digest",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        assert!(Cli::try_parse_from(valid).is_ok());

        let mut gateway = valid;
        gateway[10] = "gateway";
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
                "--tool-approval",
                "dippy",
                "--digest",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["agent-hub", "local", "authorize", "--help"]).is_err());
    }
}

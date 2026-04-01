mod client;
mod review;
mod validate;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-hub", version, about = "Agent Hub CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
        #[arg(long, default_value = "~/.config/agent-hub/tools.json")]
        config: String,

        /// Server URL (optional; enables server connectivity check)
        #[arg(long, env = "AGENT_HUB_SERVER")]
        server: Option<String>,

        /// Bearer token (optional; enables auth check when --server is provided)
        #[arg(long, env = "AGENT_HUB_TOKEN")]
        token: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.command {
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
    }
}

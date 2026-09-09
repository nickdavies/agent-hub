use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use agent_hub_approval_service as server;
use protocol::Secret;
use server::notifier::{Notifier, NullNotifier};
use server::pushover::PushoverClient;
use server::storage::{LocalFileStorage, NullStorage, Storage};
use server::webhook::WebhookClient;

#[derive(Parser)]
#[command(name = "agent-hub-approvals-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the notification server
    Serve {
        #[command(subcommand)]
        notifier: NotifierArgs,
    },
}

/// Notification backend to use.
#[derive(Subcommand)]
enum NotifierArgs {
    /// Send notifications via Pushover (https://pushover.net)
    Pushover {
        /// Pushover API token
        #[arg(long, env = "PUSHOVER_TOKEN")]
        token: String,

        /// Pushover user key
        #[arg(long, env = "PUSHOVER_USER")]
        user: String,

        #[command(subcommand)]
        storage: Option<StorageArgs>,
    },

    /// Send notifications via HTTP webhook (POST JSON to a URL)
    Webhook {
        /// URL to POST notifications to
        #[arg(long, env = "WEBHOOK_URL")]
        url: String,

        #[command(subcommand)]
        storage: Option<StorageArgs>,
    },

    /// No notifications (for localhost/Docker use with CLI approval tool)
    Noop {
        #[command(subcommand)]
        storage: Option<StorageArgs>,
    },
}

/// Storage backend configuration.
#[derive(Subcommand)]
enum StorageArgs {
    /// Persist state to a local JSON file
    LocalFile {
        /// Path to the state file
        #[arg(long, default_value = "state.json")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { notifier } => match notifier {
            NotifierArgs::Pushover {
                token,
                user,
                storage,
            } => {
                let notifier = PushoverClient::new(Secret::new(token), Secret::new(user));
                serve_with_storage(notifier, storage).await
            }
            NotifierArgs::Webhook { url, storage } => {
                let notifier = WebhookClient::new(url);
                serve_with_storage(notifier, storage).await
            }
            NotifierArgs::Noop { storage } => serve_with_storage(NullNotifier, storage).await,
        },
    }
}

async fn serve_with_storage(
    notifier: impl Notifier,
    storage: Option<StorageArgs>,
) -> anyhow::Result<()> {
    match storage {
        None => serve(notifier, NullStorage).await,
        Some(StorageArgs::LocalFile { path }) => {
            tracing::info!(?path, "using local file storage");
            serve(notifier, LocalFileStorage::new(path)).await
        }
    }
}

async fn serve(notifier: impl Notifier, storage: impl Storage) -> anyhow::Result<()> {
    server::serve_from_env(notifier, storage).await
}

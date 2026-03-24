mod error;
mod mcp;
mod server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use server::notifier::Notifier;
use server::pushover::PushoverClient;
use server::storage::{LocalFileStorage, NullStorage, Storage};
use server::webhook::WebhookClient;

#[derive(Parser)]
#[command(name = "claude-notify", version)]
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
                let notifier = PushoverClient::new(token, user);
                serve_with_storage(notifier, storage).await
            }
            NotifierArgs::Webhook { url, storage } => {
                let notifier = WebhookClient::new(url);
                serve_with_storage(notifier, storage).await
            }
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
            info!(?path, "using local file storage");
            serve(notifier, LocalFileStorage::new(path)).await
        }
    }
}

async fn serve(notifier: impl Notifier, storage: impl Storage) -> anyhow::Result<()> {
    let persisted = storage
        .load()
        .await
        .context("failed to load persisted state")?;

    let config =
        server::config::ServerConfig::from_env().context("failed to load server config")?;
    let listen_addr = config.listen_addr.clone();

    let state = server::AppState::new(config, notifier);

    if let Some(persisted) = persisted {
        info!(
            sessions = persisted.sessions.len(),
            "restoring persisted state"
        );
        state.restore(persisted).await;
    }

    // Spawn session eviction background task
    let sessions = Arc::clone(&state.sessions);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            sessions.evict_stale().await;
        }
    });

    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .context(format!("failed to bind {listen_addr}"))?;

    info!(addr = listen_addr, "server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    // Save state after graceful shutdown
    let snapshot = state.snapshot().await;
    storage
        .save(&snapshot)
        .await
        .context("failed to save state on shutdown")?;
    info!("state saved on shutdown");

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install ctrl+c handler");
    info!("shutting down");
}

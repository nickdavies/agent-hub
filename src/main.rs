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

use server::storage::{LocalFileStorage, NullStorage, Storage};

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
        Command::Serve { storage } => match storage {
            None => serve(NullStorage).await,
            Some(StorageArgs::LocalFile { path }) => {
                info!(?path, "using local file storage");
                serve(LocalFileStorage::new(path)).await
            }
        },
    }
}

async fn serve(storage: impl Storage) -> anyhow::Result<()> {
    let persisted = storage
        .load()
        .await
        .context("failed to load persisted state")?;

    let config =
        server::config::ServerConfig::from_env().context("failed to load server config")?;
    let listen_addr = config.listen_addr.clone();

    let state = server::AppState::new(config);

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

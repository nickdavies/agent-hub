mod error;
mod grpc;
mod mcp;
mod server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rusqlite::{Connection, TransactionBehavior};
use tracing::info;
use tracing_subscriber::EnvFilter;

use protocol::{Secret, SessionStatus};
use server::config::ApprovalFeatureMode;
use server::notifier::{Notifier, NullNotifier};
use server::oauth::OAuthManager;
use server::pushover::PushoverClient;
use server::storage::{LocalFileStorage, NullStorage, Storage};
use server::webhook::WebhookClient;

#[derive(Parser)]
#[command(name = "agent-hub-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Refuse a rollback to a v1-only server while v2 work or sessions remain.
    RollbackPreflightV1 {
        #[arg(long, env = "ORCHESTRATION_DB")]
        database: PathBuf,
    },
    /// Refuse a rollback to a v2-only server while v3 work or sessions remain.
    RollbackPreflightV2 {
        #[arg(long, env = "ORCHESTRATION_DB")]
        database: PathBuf,
    },
    /// Explicitly change the durable authorization protocol fence.
    SetAuthorizationProtocolMode {
        #[arg(long, env = "ORCHESTRATION_DB")]
        database: PathBuf,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3))]
        max_version: u32,
    },
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
        Command::RollbackPreflightV1 { database } => rollback_preflight_v1(&database),
        Command::RollbackPreflightV2 { database } => rollback_preflight_v2(&database),
        Command::SetAuthorizationProtocolMode {
            database,
            max_version,
        } => set_authorization_protocol_mode(&database, max_version),
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

fn rollback_preflight_v1(database: &std::path::Path) -> anyhow::Result<()> {
    rollback_preflight(database, 1)
}

fn rollback_preflight_v2(database: &std::path::Path) -> anyhow::Result<()> {
    rollback_preflight(database, 2)
}

fn rollback_preflight(database: &std::path::Path, target_version: u32) -> anyhow::Result<()> {
    let rejected_version = target_version + 1;
    let mut connection = Connection::open(database)
        .with_context(|| format!("open orchestration database {}", database.display()))?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    let updated = tx.execute(
        "UPDATE authorization_protocol_mode SET max_authorization_version = ?1, updated_at = ?2 WHERE id = 1",
        rusqlite::params![target_version, chrono::Utc::now().to_rfc3339()],
    )?;
    if updated != 1 {
        anyhow::bail!("authorization protocol fence row is missing or malformed");
    }
    let higher_jobs: u64 = tx.query_row(
        "SELECT COUNT(*) FROM jobs WHERE canonical_authorization_version >= ?1 AND state IN ('pending','assigned','running')",
        [rejected_version],
        |row| row.get(0),
    )?;
    let capable_sessions: u64 = tx.query_row(
        "SELECT COUNT(*) FROM workers WHERE (?1 <= 2 AND protocol_capabilities_json LIKE '%canonical_authorization_v2%') OR protocol_capabilities_json LIKE '%canonical_authorization_v3%'",
        [rejected_version],
        |row| row.get(0),
    )?;
    let higher_obligations: u64 = tx.query_row(
        "SELECT COUNT(*) FROM authorization_obligations WHERE canonical_authorization_version >= ?1 AND state IN ('pending','approved')",
        [rejected_version], |row| row.get(0),
    )?;
    let higher_records: u64 = tx.query_row(
        "SELECT COUNT(*) FROM authorization_records r JOIN work_items w ON w.id = r.work_item_id WHERE r.canonical_authorization_version >= ?1 AND w.state NOT IN ('succeeded','failed','cancelled')",
        [rejected_version], |row| row.get(0),
    )?;
    let higher_recovery: u64 = tx.query_row(
        "SELECT COUNT(*) FROM worker_session_recovery_inbox i JOIN jobs j ON j.id = i.job_id WHERE j.canonical_authorization_version >= ?1 AND i.state IN ('pending','approved')",
        [rejected_version], |row| row.get(0),
    )?;
    let reactivatable_workflows: u64 = tx.query_row(
        "SELECT COUNT(*) FROM work_items WHERE state NOT IN ('succeeded','failed','cancelled','human_gate')",
        [], |row| row.get(0),
    )?;
    if [
        higher_jobs,
        capable_sessions,
        higher_obligations,
        higher_records,
        higher_recovery,
        reactivatable_workflows,
    ]
    .into_iter()
    .any(|count| count != 0)
    {
        let ids = tx.prepare(
            "SELECT id FROM jobs WHERE canonical_authorization_version >= ?1 AND state IN ('pending','assigned','running') LIMIT 10"
        )?.query_map([rejected_version], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::bail!(
            "v{target_version} rollback refused: active_higher_jobs={higher_jobs}, higher_capability_workers={capable_sessions}, higher_obligations={higher_obligations}, reactivatable_higher_records={higher_records}, higher_recovery={higher_recovery}, reactivatable_workflows={reactivatable_workflows}, bounded_job_ids={ids:?}"
        );
    }
    tx.commit()?;
    println!("v{target_version} rollback preflight passed");
    Ok(())
}

fn set_authorization_protocol_mode(
    database: &std::path::Path,
    max_version: u32,
) -> anyhow::Result<()> {
    if max_version == 1 {
        return rollback_preflight_v1(database);
    }
    if max_version == 2 {
        return rollback_preflight_v2(database);
    }
    let mut connection = Connection::open(database)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    let updated = tx.execute(
        "UPDATE authorization_protocol_mode SET max_authorization_version = 3, updated_at = ?1 WHERE id = 1",
        [chrono::Utc::now().to_rfc3339()],
    )?;
    if updated != 1 {
        anyhow::bail!("authorization protocol fence row is missing or malformed");
    }
    tx.commit()?;
    println!("authorization protocol fence set to v3");
    Ok(())
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
    let grpc_listen_addr = config.grpc_listen_addr.clone();

    // Initialize OAuth if approval mode requires it (skip when auth is disabled)
    let oauth = if config.approval_mode != ApprovalFeatureMode::Disabled
        && config.auth_mode != server::config::AuthMode::None
    {
        let base_url = config
            .base_url
            .as_ref()
            .expect("BASE_URL validated in config");
        let oauth = OAuthManager::from_env(base_url)
            .await
            .context("failed to initialize OAuth")?;

        if oauth.is_none() {
            anyhow::bail!(
                "APPROVAL_MODE={:?} requires at least one auth provider (GOOGLE_CLIENT_ID/SECRET, OIDC_ISSUER_URL/CLIENT_ID/SECRET, or BASIC_AUTH_USER/PASSWORD)",
                config.approval_mode
            );
        }
        oauth
    } else {
        None
    };

    let state = server::AppState::new(config, notifier, oauth);
    server::artifact_store::ensure_store_root_exists(&state.config.artifact_store_root)
        .context("failed to initialize artifact store root")?;
    state
        .orchestration
        .init()
        .await
        .context("failed to initialize orchestration storage")?;

    if let Some(persisted) = persisted {
        info!(
            sessions = persisted.sessions.len(),
            "restoring persisted state"
        );
        state.restore(persisted).await;
    }

    // Spawn session eviction background task
    let sessions = Arc::clone(&state.sessions);
    let approvals = Arc::clone(&state.approvals);
    let questions = Arc::clone(&state.questions);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let evicted = sessions.evict_stale(Duration::from_secs(1800)).await;
            for session_id in &evicted {
                approvals.evict_session(session_id).await;
                questions.evict_session(session_id).await;
            }
            // Cancel approvals whose gateway has stopped polling (killed, crashed,
            // or lost connectivity). Threshold is 2× the 55s long-poll window.
            let orphaned = approvals.evict_orphaned(Duration::from_secs(120)).await;
            if orphaned > 0 {
                info!(count = orphaned, "cancelled orphaned approvals");
            }
            let orphaned_q = questions.evict_orphaned(Duration::from_secs(120)).await;
            if orphaned_q > 0 {
                info!(count = orphaned_q, "cancelled orphaned questions");
            }
            // After orphaned eviction, any session still in Waiting state but with no
            // pending question or approval is a zombie (agent died mid-question).
            // Reset it to Idle so normal TTL eviction can clean it up.
            let waiting_sessions = sessions
                .list()
                .await
                .into_iter()
                .filter(|s| s.stored_status == SessionStatus::Waiting)
                .collect::<Vec<_>>();
            for s in waiting_sessions {
                let has_pending_approval = approvals
                    .first_pending_for_session(&s.session_id)
                    .await
                    .is_some();
                let has_pending_question = questions
                    .first_pending_for_session(&s.session_id)
                    .await
                    .is_some();
                if !has_pending_approval && !has_pending_question {
                    info!(
                        session_id = %s.session_id,
                        "resetting zombie Waiting session to Idle"
                    );
                    sessions
                        .set_status(&s.session_id, SessionStatus::Idle, None, None)
                        .await;
                }
            }
        }
    });

    // Spawn orchestration maintenance loop (faster cadence than session cleanup)
    let orchestration_bg = Arc::clone(&state.orchestration);
    let worker_recovery_notify = orchestration_bg.worker_recovery_notify();
    let job_events_bg = Arc::clone(&state.job_events);
    let maintenance_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = worker_recovery_notify.notified() => {}
            }
            tracing::debug!("maintenance tick starting");
            match tokio::time::timeout(
                Duration::from_secs(120),
                orchestration_bg.maintenance_tick(),
            )
            .await
            {
                Ok(Ok(stats)) => {
                    if stats.requeued_jobs > 0
                        || stats.created_jobs > 0
                        || stats.lease_requeued_jobs > 0
                        || stats.github_fired_events > 0
                    {
                        info!(
                            requeued_jobs = stats.requeued_jobs,
                            created_jobs = stats.created_jobs,
                            lease_requeued_jobs = stats.lease_requeued_jobs,
                            github_fired_events = stats.github_fired_events,
                            "orchestration maintenance tick"
                        );
                    }
                }
                Ok(Err(error)) => {
                    tracing::warn!("orchestration maintenance failed: {error}");
                }
                Err(_) => {
                    tracing::warn!("orchestration maintenance tick timed out after 120s");
                }
            }
            let cleaned = job_events_bg.cleanup_terminal_expired().await;
            if cleaned > 0 {
                tracing::debug!(
                    count = cleaned,
                    "cleaned expired terminal job event buffers"
                );
            }
        }

        #[allow(unreachable_code)]
        {
            tracing::error!("maintenance loop exited unexpectedly");
        }
    });

    tokio::spawn(async move {
        match maintenance_handle.await {
            Ok(()) => {
                tracing::error!("maintenance loop exited cleanly (should never happen)");
            }
            Err(error) => {
                tracing::error!("maintenance loop panicked: {error}");
            }
        }
    });

    let app = server::router(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .context(format!("failed to bind {listen_addr}"))?;

    info!(addr = listen_addr, "server listening");

    if let Some(grpc_addr) = grpc_listen_addr {
        let orchestration = Arc::clone(&state.orchestration);
        let job_events = Arc::clone(&state.job_events);
        let grpc_config = Arc::clone(&state.config);
        tokio::spawn(async move {
            let addr = match grpc_addr.parse() {
                Ok(addr) => addr,
                Err(error) => {
                    tracing::warn!("invalid GRPC_LISTEN_ADDR: {error}");
                    return;
                }
            };
            let svc = grpc::WorkerLifecycleGrpc::new(orchestration, job_events);
            let interceptor = grpc::GrpcAuthInterceptor::new(grpc_config);
            let server = tonic::transport::Server::builder().add_service(
                grpc::pb::worker_lifecycle_server::WorkerLifecycleServer::with_interceptor(
                    svc,
                    interceptor,
                ),
            );
            tracing::info!(addr = %addr, "gRPC worker service listening");
            if let Err(error) = server.serve(addr).await {
                tracing::warn!("gRPC server failed: {error}");
            }
        });
    }

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

#[cfg(test)]
mod rollback_preflight_tests {
    use super::*;

    #[tokio::test]
    async fn rollback_preflight_rejects_v2_jobs_and_sessions_then_allows_v1() {
        let path = std::env::temp_dir().join(format!(
            "rollback-preflight-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let tasks = std::env::temp_dir().join(format!("rollback-tasks-{}", uuid::Uuid::new_v4()));
        let service = server::orchestration::OrchestrationService::new(
            Arc::new(server::workflows::WorkflowCatalog::empty_for_test()),
            Arc::new(server::task_files::TaskFileStore::new(tasks.clone())),
            path.to_string_lossy().into_owned(),
        );
        service.init().await.unwrap();
        drop(service);
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT max_authorization_version FROM authorization_protocol_mode WHERE id = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap(),
            3,
            "new databases explicitly initialize the current v3 fence"
        );
        connection.execute(
            "INSERT INTO workers (id, hostname, state, supported_modes_json, protocol_capabilities_json, session_id, session_generation, last_heartbeat_at) VALUES ('worker-v2','host','idle','[]','[\"canonical_authorization_v2\"]','session',1,'now')",
            [],
        ).unwrap();
        assert!(rollback_preflight_v1(&path).is_err());
        connection.execute("DELETE FROM workers", []).unwrap();
        assert!(rollback_preflight_v1(&path).is_ok());
        assert_eq!(
            connection
                .query_row(
                    "SELECT max_authorization_version FROM authorization_protocol_mode WHERE id = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap(),
            1
        );
        assert!(
            connection.execute(
                "INSERT INTO authorization_obligations (checkpoint_owner_id, work_item_id, request_digest, request_json, job_json, canonical_authorization_version, canonical_authorization_bytes, state, created_at, updated_at) VALUES ('owner','item','digest','{}','{}',2,X'00','pending','now','now')",
                [],
            ).is_err(),
            "v2 obligation insertion must lose the race against a committed v1 fence"
        );
        set_authorization_protocol_mode(&path, 2).unwrap();
        assert_eq!(
            connection.execute(
                "INSERT INTO authorization_obligations (checkpoint_owner_id, work_item_id, request_digest, request_json, job_json, canonical_authorization_version, canonical_authorization_bytes, state, created_at, updated_at) VALUES ('owner','item','digest','{}','{}',2,X'00','pending','now','now')",
                [],
            ).unwrap(),
            1
        );
        connection
            .execute("DELETE FROM authorization_obligations", [])
            .unwrap();
        set_authorization_protocol_mode(&path, 3).unwrap();
        connection.execute(
            "INSERT INTO workers (id, hostname, state, supported_modes_json, protocol_capabilities_json, session_id, session_generation, last_heartbeat_at) VALUES ('worker-v3','host','idle','[]','[\"canonical_authorization_v3\"]','session-v3',1,'now')",
            [],
        ).unwrap();
        assert!(rollback_preflight_v2(&path).is_err());
        assert_eq!(
            connection
                .query_row(
                    "SELECT max_authorization_version FROM authorization_protocol_mode WHERE id = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap(),
            3,
            "failed rollback must preserve the durable v3 fence"
        );
        connection.execute("DELETE FROM workers", []).unwrap();
        assert!(rollback_preflight_v2(&path).is_ok());
        assert_eq!(
            connection
                .query_row(
                    "SELECT max_authorization_version FROM authorization_protocol_mode WHERE id = 1",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .unwrap(),
            2
        );
        connection
            .execute("DELETE FROM authorization_protocol_mode", [])
            .unwrap();
        assert!(set_authorization_protocol_mode(&path, 2).is_err());
        drop(connection);
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_dir_all(tasks);
    }
}

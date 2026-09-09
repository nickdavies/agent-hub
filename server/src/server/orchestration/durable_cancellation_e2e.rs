use super::*;
use crate::grpc::{WorkerLifecycleGrpc, pb as worker_pb};
use crate::server::config::{AuthMode, ServerConfig};
use crate::server::job_events::JobEventHub;
use anyhow::{Context, ensure};
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const E2E_TIMEOUT: Duration = Duration::from_secs(120);

struct E2eGuard {
    worker: Child,
    worker_id: String,
    http_task: Option<tokio::task::JoinHandle<()>>,
    grpc_task: Option<tokio::task::JoinHandle<()>>,
    root: PathBuf,
    database: PathBuf,
    tasks: PathBuf,
}

impl Drop for E2eGuard {
    fn drop(&mut self) {
        let _ = self.worker.kill();
        let _ = self.worker.wait();
        if let Some(task) = self.http_task.take() {
            task.abort();
        }
        if let Some(task) = self.grpc_task.take() {
            task.abort();
        }
        if let Ok(output) = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=com.reddit.agent-hub.worker={}", self.worker_id),
            ])
            .output()
        {
            let ids = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                let _ = Command::new("docker")
                    .args(["rm", "-f", "-v"])
                    .args(ids)
                    .status();
            }
        }
        if let Ok(output) = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=agent-hub.local-agent.worker_id={}", self.worker_id),
            ])
            .output()
        {
            let ids = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                let _ = Command::new("docker")
                    .args(["rm", "-f", "-v"])
                    .args(ids)
                    .status();
            }
        }
        for tier in ["build", "llm", "external"] {
            let _ = Command::new("docker")
                .args([
                    "network",
                    "rm",
                    &format!("agent-hub-{}-egress-{tier}", self.worker_id),
                ])
                .output();
        }
        if std::env::var("AGENT_HUB_E2E_PRESERVE_ROOT").as_deref() != Ok("1") {
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_file(&self.database);
            let mut writer_lock = self.database.as_os_str().to_os_string();
            writer_lock.push(".writer.lock");
            let _ = std::fs::remove_file(PathBuf::from(writer_lock));
            let _ = std::fs::remove_dir_all(&self.tasks);
        }
    }
}

impl E2eGuard {
    async fn stop_transports(&mut self) {
        if let Some(task) = self.http_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.grpc_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "tester-managed: requires Docker, a cfg(test) local-agent binary, and trusted no-secret preflight config"]
async fn private_planner_public_plural_cancel_real_grpc_worker_docker() -> anyhow::Result<()> {
    run_subprocess_case(None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "tester-managed: requires Docker, a test-fault-hooks local-agent binary, and trusted no-secret config"]
async fn lifecycle_pause_restart_rebind_matrix() -> anyhow::Result<()> {
    const CASES: &[(&str, bool)] = &[
        ("lifecycle:workspace_preparing:before", false),
        ("lifecycle:workspace_preparing:after", false),
        ("lifecycle:workspace_prepared:after", false),
        ("lifecycle:credentials_resolving:before", false),
        ("lifecycle:credentials_resolving:after", false),
        ("lifecycle:credentials_resolved:after", false),
        ("lifecycle:docker_create_submitting:before", false),
        ("lifecycle:docker_create_submitting:after", false),
        ("lifecycle:docker_create_submitted:before", false),
        ("lifecycle:docker_create_submitted:after", false),
        ("lifecycle:docker_start_submitting:before", false),
        ("lifecycle:docker_start_submitting:after", false),
        ("lifecycle:docker_started:before", false),
        ("lifecycle:docker_started:after", false),
        ("lifecycle:running:before", false),
        ("lifecycle:running:after", false),
        ("lifecycle:cleanup_running:before", true),
        ("lifecycle:cleanup_running:after", true),
        ("lifecycle:terminal_reporting:before", true),
        ("lifecycle:terminal_reporting:after", true),
    ];
    let mut failures = Vec::new();
    for (boundary, cancel_before_pause) in CASES {
        if let Ok(selected) = std::env::var("AGENT_HUB_E2E_FAULT_CASE")
            && selected != *boundary
        {
            continue;
        }
        let result = run_subprocess_case(Some(FaultCase {
            boundary,
            cancel_before_pause: *cancel_before_pause,
        }))
        .await;
        match result {
            Ok(()) => eprintln!("PASS {boundary}"),
            Err(error) => {
                eprintln!("FAIL {boundary}: {error:#}");
                failures.push(format!("{boundary}: {error:#}"));
            }
        }
    }
    ensure!(failures.is_empty(), "matrix failures: {failures:#?}");
    Ok(())
}

#[derive(Clone, Copy)]
struct FaultCase {
    boundary: &'static str,
    cancel_before_pause: bool,
}

async fn run_subprocess_case(fault: Option<FaultCase>) -> anyhow::Result<()> {
    ensure!(
        std::env::var("AGENT_HUB_DURABLE_CANCELLATION_E2E").as_deref() == Ok("1"),
        "set AGENT_HUB_DURABLE_CANCELLATION_E2E=1"
    );
    let worker_binary = PathBuf::from(
        std::env::var("AGENT_HUB_E2E_LOCAL_AGENT_BIN")
            .context("AGENT_HUB_E2E_LOCAL_AGENT_BIN is required")?,
    );
    let trusted_config = PathBuf::from(
        std::env::var("AGENT_HUB_E2E_CONFIG_ROOT")
            .context("AGENT_HUB_E2E_CONFIG_ROOT is required")?,
    );
    ensure!(worker_binary.is_file(), "worker binary is missing");
    ensure!(
        trusted_config.join("local-agent.json").is_file(),
        "trusted local-agent config is missing"
    );
    let image = std::env::var("AGENT_HUB_E2E_IMAGE")
        .unwrap_or_else(|_| "agent-hub/job-code:2026-08-18".to_string());
    let catalog = generic_assertion_catalog(&image)?;
    let harness = AuthorizedWorkflowHarness::new(catalog, "cancellation_e2e", false).await;
    let (svc, adapter, work_item_id, database, tasks) = harness.into_e2e_parts();
    let catalog = Arc::clone(&svc.workflows);
    let mut orchestration = Arc::new(svc);
    let job_events = Arc::new(JobEventHub::new());

    let root = std::env::temp_dir().join(format!(
        "durable-cancellation-private-e2e-{}",
        Uuid::new_v4().simple()
    ));
    let artifacts = root.join("artifacts");
    let worker_data = root.join("worker-data");
    std::fs::create_dir_all(&artifacts)?;
    std::fs::create_dir_all(&worker_data)?;

    let mut state = e2e_app_state(Arc::clone(&orchestration), Arc::clone(&job_events), &root)?;
    state.orchestration = Arc::clone(&orchestration);
    state.job_events = Arc::clone(&job_events);
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut http_addr = http_listener.local_addr()?;
    let http_task = tokio::spawn(async move {
        let _ = axum::serve(http_listener, crate::server::router(state)).await;
    });

    let mut grpc_addr = StdTcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let grpc_service = WorkerLifecycleGrpc::new(Arc::clone(&orchestration), job_events);
    let grpc_task = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(
                worker_pb::worker_lifecycle_server::WorkerLifecycleServer::new(grpc_service),
            )
            .serve(grpc_addr)
            .await;
    });

    let worker_id = format!("cancel-e2e-{}", Uuid::new_v4().simple());
    let docker_before = owned_container_ids(&worker_id)?;
    ensure!(
        docker_before.is_empty(),
        "unique worker Docker pre-state is not empty"
    );
    let ready = root.join("fault-ready");
    let release = root.join("fault-release");
    let worker = spawn_worker(
        &worker_binary,
        &trusted_config,
        &worker_id,
        grpc_addr,
        http_addr,
        &worker_data,
        &artifacts,
        &root.join("worker.log"),
        fault.map(|case| (case.boundary, ready.as_path(), release.as_path())),
    )?;
    let mut guard = E2eGuard {
        worker,
        worker_id: worker_id.clone(),
        http_task: Some(http_task),
        grpc_task: Some(grpc_task),
        root,
        database: database.clone(),
        tasks,
    };

    let running = wait_for_running_job(
        &orchestration,
        work_item_id,
        &guard.root.join("worker.log"),
        &mut guard.worker,
    )
    .await?;
    if fault.is_none() {
        wait_for_owned_container(&worker_id).await?;
    }
    if fault.is_some_and(|case| case.cancel_before_pause) {
        post_public_cancel(http_addr, work_item_id).await?;
    }
    if fault.is_some() {
        wait_for_fault_ready(
            &ready,
            &guard.root.join("worker.log"),
            &mut guard.worker,
            &orchestration,
            work_item_id,
        )
        .await?;
    }
    guard.worker.kill()?;
    guard.worker.wait()?;
    if !fault.is_some_and(|case| case.cancel_before_pause) {
        post_public_cancel(http_addr, work_item_id).await?;
    }
    Connection::open(&database)?.execute(
        "UPDATE workers SET last_heartbeat_at = ?2 WHERE id = ?1",
        rusqlite::params![
            worker_id,
            (Utc::now()
                - chrono::TimeDelta::seconds(
                    i64::try_from(STALE_WORKER_SECS + 1).unwrap_or(i64::MAX),
                ))
            .to_rfc3339(),
        ],
    )?;
    guard.stop_transports().await;
    orchestration.release_writer_guard_for_restart_test()?;
    drop(orchestration);
    let restarted = OrchestrationService::new(
        Arc::clone(&catalog),
        Arc::clone(&adapter),
        database.to_string_lossy().into_owned(),
    )
    .with_scm_hosts(test_scm_registry());
    restarted
        .init()
        .await
        .context("restart orchestration service")?;
    orchestration = Arc::new(restarted);
    let restarted_events = Arc::new(JobEventHub::new());
    let mut restarted_state = e2e_app_state(
        Arc::clone(&orchestration),
        Arc::clone(&restarted_events),
        &guard.root,
    )?;
    restarted_state.orchestration = Arc::clone(&orchestration);
    restarted_state.job_events = Arc::clone(&restarted_events);
    let restarted_http = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    http_addr = restarted_http.local_addr()?;
    guard.http_task = Some(tokio::spawn(async move {
        let _ = axum::serve(restarted_http, crate::server::router(restarted_state)).await;
    }));
    grpc_addr = StdTcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let restarted_grpc = WorkerLifecycleGrpc::new(Arc::clone(&orchestration), restarted_events);
    guard.grpc_task = Some(tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(
                worker_pb::worker_lifecycle_server::WorkerLifecycleServer::new(restarted_grpc),
            )
            .serve(grpc_addr)
            .await;
    }));
    guard.worker = spawn_worker(
        &worker_binary,
        &trusted_config,
        &worker_id,
        grpc_addr,
        http_addr,
        &worker_data,
        &artifacts,
        &guard.root.join("replacement-worker.log"),
        None,
    )?;

    wait_for_terminal_cancellation(
        &orchestration,
        work_item_id,
        running.id,
        &guard.root.join("replacement-worker.log"),
        &mut guard.worker,
    )
    .await?;
    ensure!(
        owned_container_ids(&worker_id)?.is_empty(),
        "owned container remains before acknowledgement"
    );
    wait_for_terminal_cancellation_invariants(
        &database,
        work_item_id,
        running.id,
        &worker_id,
        &worker_data,
    )
    .await?;

    guard.worker.kill()?;
    guard.worker.wait()?;
    guard.stop_transports().await;
    orchestration.release_writer_guard_for_restart_test()?;
    drop(orchestration);
    let reopened = OrchestrationService::new(
        Arc::clone(&catalog),
        Arc::clone(&adapter),
        database.to_string_lossy().into_owned(),
    )
    .with_scm_hosts(test_scm_registry());
    reopened
        .init()
        .await
        .context("reopen acknowledged cancellation")?;
    let reopened_job = reopened
        .get_job(running.id)
        .await?
        .context("acknowledged job missing after reopen")?;
    let reopened_item = reopened
        .get_work_item(work_item_id)
        .await?
        .context("cancelled item missing after reopen")?;
    ensure!(
        reopened_job.state == JobState::Cancelled
            && reopened_item.state == WorkItemState::Cancelled,
        "acknowledged cancellation did not survive reopen"
    );
    reopened.release_writer_guard_for_restart_test()?;
    drop(reopened);

    wait_for_terminal_cancellation_invariants(
        &database,
        work_item_id,
        running.id,
        &worker_id,
        &worker_data,
    )
    .await?;
    ensure!(
        owned_container_ids(&worker_id)? == docker_before,
        "worker Docker resource hash differs from pre-state"
    );
    Ok(())
}

async fn post_public_cancel(
    http_addr: std::net::SocketAddr,
    work_item_id: Uuid,
) -> anyhow::Result<()> {
    let response = reqwest::Client::new()
        .post(format!(
            "http://{http_addr}/api/v1/orchestration/work-items/{work_item_id}/events"
        ))
        .json(&serde_json::json!({"event":"cancel"}))
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "public plural cancel failed: {}",
        response.text().await?
    );
    Ok(())
}

async fn wait_for_fault_ready(
    path: &Path,
    log: &Path,
    worker: &mut Child,
    orchestration: &OrchestrationService,
    work_item_id: Uuid,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + E2E_TIMEOUT;
    loop {
        if let Some(status) = worker.try_wait()? {
            anyhow::bail!(
                "worker exited before fault pause with {status}: {}",
                std::fs::read_to_string(log)
                    .unwrap_or_else(|error| format!("<unreadable log: {error}>"))
            );
        }
        if path.is_file() {
            return Ok(());
        }
        if let Some(item) = orchestration.get_work_item(work_item_id).await?
            && matches!(
                item.state,
                WorkItemState::Succeeded | WorkItemState::Failed | WorkItemState::Cancelled
            )
        {
            anyhow::bail!(
                "work item became terminal before requested durable fault boundary: {:?}",
                item.state
            );
        }
        ensure!(Instant::now() < deadline, "fault pause timeout");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn assert_terminal_cancellation_invariants(
    database: &Path,
    work_item_id: Uuid,
    job_id: Uuid,
    worker_id: &str,
    worker_data: &Path,
) -> anyhow::Result<()> {
    let connection = Connection::open(database)?;
    let row: (
        String,
        u64,
        String,
        String,
        String,
        u32,
        String,
        String,
        String,
    ) = connection.query_row(
        "SELECT cancellation_id, generation, invocation_id, assignment_id,
                    runtime_assignment_id, attempt, worker_id, worker_session_id,
                    acknowledgement_json
             FROM job_cancellation_commands WHERE job_id = ?1 AND delivery_state = 'acknowledged'",
        [job_id.to_string()],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        },
    )?;
    let acknowledgement: protocol::CancellationAcknowledgement = serde_json::from_str(&row.8)?;
    ensure!(
        acknowledgement.command.generation == row.1,
        "rebind generation mismatch"
    );
    protocol::verify_cancellation_assignment_id(&acknowledgement.command)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        acknowledgement.command.runtime_assignment_id == Uuid::parse_str(&row.4)?,
        "runtime assignment was not preserved"
    );
    let result_json: String = connection.query_row(
        "SELECT result_json FROM jobs WHERE id = ?1 AND state = 'cancelled'",
        [job_id.to_string()],
        |row| row.get(0),
    )?;
    let result: protocol::JobResult = serde_json::from_str(&result_json)?;
    ensure!(
        result.cancellation_acknowledgement.as_ref() == Some(&acknowledgement),
        "command, acknowledgement, and result identities differ"
    );
    ensure!(
        matches!(
            acknowledgement.containment.process,
            protocol::ContainmentStatus::VerifiedAbsent
                | protocol::ContainmentStatus::NotApplicable
        ) && matches!(
            acknowledgement.containment.container,
            protocol::ContainmentStatus::VerifiedAbsent
                | protocol::ContainmentStatus::NotApplicable
        ),
        "containment evidence is not positive"
    );
    let completed: u32 = connection.query_row(
        "SELECT COUNT(*) FROM work_item_cancellations WHERE work_item_id = ?1 AND state = 'completed'",
        [work_item_id.to_string()],
        |row| row.get(0),
    )?;
    let receipt: u32 = connection.query_row(
        "SELECT COUNT(*) FROM job_output_receipts WHERE job_id = ?1 AND application_state = 'applied'",
        [job_id.to_string()],
        |row| row.get(0),
    )?;
    let outbox: u32 = connection.query_row(
        "SELECT COUNT(*) FROM terminal_completion_outbox WHERE job_id = ?1 AND state = 'applied'",
        [job_id.to_string()],
        |row| row.get(0),
    )?;
    let late: u32 = connection.query_row(
        "SELECT COUNT(*) FROM cancellation_late_results WHERE job_id = ?1",
        [job_id.to_string()],
        |row| row.get(0),
    )?;
    let active: u32 = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM job_cancellation_commands WHERE job_id = ?1 AND delivery_state != 'acknowledged') +
                (SELECT COUNT(*) FROM work_item_cancellations WHERE work_item_id = ?2 AND state != 'completed')",
        rusqlite::params![job_id.to_string(), work_item_id.to_string()],
        |row| row.get(0),
    )?;
    ensure!(
        completed == 1 && receipt == 1 && outbox == 1 && late == 0 && active == 0,
        "durable terminal audit invariant failed"
    );
    let attempts = worker_data.join("attempts");
    if attempts.exists() {
        for entry in std::fs::read_dir(attempts)? {
            let entry = entry?;
            ensure!(
                entry.file_name() == "reconciliation",
                "private attempt state remains after acknowledgement"
            );
            let journal = entry.path().join("external-effects.jsonl");
            if journal.exists() {
                let mut latest = std::collections::HashMap::new();
                for line in std::fs::read_to_string(journal)?.lines() {
                    let record: serde_json::Value = serde_json::from_str(line)?;
                    let operation = record["operation_id"]
                        .as_str()
                        .context("reconciliation operation has no ID")?;
                    let status = record["status"]
                        .as_str()
                        .context("reconciliation operation has no status")?;
                    latest.insert(operation.to_string(), status.to_string());
                }
                ensure!(
                    latest
                        .values()
                        .all(|status| matches!(status.as_str(), "completed" | "reconciled")),
                    "reconciliation private state has pending cleanup operations after acknowledgement: {latest:?}"
                );
            }
        }
    }
    let runtime_attempts = worker_data.join("runtime-attempts");
    if runtime_attempts.exists() {
        ensure!(
            std::fs::read_dir(&runtime_attempts)?.next().is_none(),
            "runtime-attempt or MCP state remains after acknowledgement"
        );
    }
    ensure!(
        owned_container_ids(worker_id)?.is_empty(),
        "owned Docker container remains after acknowledgement"
    );
    for path in [
        worker_data.join("docker-jobs"),
        worker_data.join("opencode-jobs"),
    ] {
        if path.exists() {
            ensure!(
                std::fs::read_dir(&path)?.next().is_none(),
                "worker cleanup resource remains at {}",
                path.display()
            );
        }
    }
    let worktrees = worker_data.join("worktrees");
    if worktrees.exists() {
        for entry in std::fs::read_dir(&worktrees)? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_dir() && std::fs::read_dir(entry.path())?.next().is_none(),
                "live workspace remains at {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

async fn wait_for_terminal_cancellation_invariants(
    database: &Path,
    work_item_id: Uuid,
    job_id: Uuid,
    worker_id: &str,
    worker_data: &Path,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + E2E_TIMEOUT;
    loop {
        match assert_terminal_cancellation_invariants(
            database,
            work_item_id,
            job_id,
            worker_id,
            worker_data,
        ) {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    binary: &Path,
    config_root: &Path,
    worker_id: &str,
    grpc_addr: std::net::SocketAddr,
    http_addr: std::net::SocketAddr,
    worker_data: &Path,
    artifacts: &Path,
    log_path: &Path,
    fault: Option<(&str, &Path, &Path)>,
) -> anyhow::Result<Child> {
    let log = std::fs::File::create(log_path)?;
    let mut command = Command::new(binary);
    command
        .args([
            "--transport",
            "grpc",
            "--grpc-endpoint",
            &format!("http://{grpc_addr}"),
            "--server",
            &format!("http://{http_addr}"),
            "--worker-id",
            worker_id,
            "--hostname",
            "localhost",
            "--data-dir",
            worker_data.to_str().context("worker data UTF-8")?,
            "--artifact-store-root",
            artifacts.to_str().context("artifact root UTF-8")?,
            "--poll-interval-secs",
            "1",
        ])
        .env("CONFIG_ROOT", config_root)
        .env("AGENT_HUB_TEST_SKIP_EGRESS_PREFLIGHT", "1")
        .env("GITHUB_READ_TOKEN", "non-secret-test-value")
        .env("GITHUB_ENTERPRISE_READ_TOKEN", "non-secret-test-value")
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if let Some((boundary, ready, release)) = fault {
        command
            .env(
                "AGENT_HUB_TEST_EXECUTION_FAULT",
                format!("pause:{boundary}"),
            )
            .env("AGENT_HUB_TEST_EXECUTION_FAULT_READY", ready)
            .env("AGENT_HUB_TEST_EXECUTION_FAULT_RELEASE", release);
    }
    command.spawn().context("spawn real gRPC worker")
}

fn generic_assertion_catalog(image: &str) -> anyhow::Result<Arc<WorkflowCatalog>> {
    let root = std::env::temp_dir().join(format!("cancel-e2e-workflow-{}", Uuid::new_v4()));
    std::fs::create_dir_all(root.join("cancellation_e2e"))?;
    std::fs::write(
        root.join("cancellation_e2e/workflow.toml"),
        format!(
            r#"[workflow]
id = "cancellation_e2e"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "command"
executor = "docker"
timeout_secs = 120
repo_access = "read_write"
container_image = {image:?}
command = "/bin/sh"
args = ["-c", "printf durable-cancellation-marker; trap '' TERM; while :; do sleep 1; done"]

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#
        ),
    )?;
    Ok(Arc::new(WorkflowCatalog::load_from_dir(
        root.to_str().context("workflow path UTF-8")?,
    )?))
}

fn e2e_app_state(
    orchestration: Arc<OrchestrationService>,
    job_events: Arc<JobEventHub>,
    root: &Path,
) -> anyhow::Result<crate::server::AppState> {
    let config_root = root.join("server-config");
    let workflows = root.join("empty-workflows");
    let tasks = root.join("http-tasks");
    std::fs::create_dir_all(&config_root)?;
    std::fs::create_dir_all(&workflows)?;
    std::fs::create_dir_all(&tasks)?;
    unsafe { std::env::set_var("SCM_CANCELLATION_E2E_READ_TOKEN", "non-secret-test-value") };
    std::fs::write(
        config_root.join("system.toml"),
        "[system.images]\ninstalled=[]\n[system.network_policies]\ninstalled=[]\n[system.environments]\ninstalled=[]\n[[system.scm_hosts]]\nkind='github'\nhost='github.com'\napi_base_url='https://api.github.com'\nread_token_env='SCM_CANCELLATION_E2E_READ_TOKEN'\n",
    )?;
    std::fs::write(config_root.join("local-agent.json"), "{}")?;
    let config = ServerConfig {
        auth_mode: AuthMode::None,
        tokens: vec![],
        listen_addr: "127.0.0.1:0".into(),
        base_url: None,
        approvals_server: None,
        approvals_token: None,
        workflows_dir: workflows.to_string_lossy().into_owned(),
        config_root: config_root.to_string_lossy().into_owned(),
        task_dir: tasks.to_string_lossy().into_owned(),
        task_state_dir: tasks.with_extension("state").to_string_lossy().into_owned(),
        task_adapter: "file".into(),
        vikunja_base_url: None,
        vikunja_token: None,
        vikunja_bot_username: None,
        vikunja_bot_password: None,
        vikunja_project_id: None,
        vikunja_label_prefix: None,
        orchestration_db_path: ":memory:".into(),
        artifact_store_root: root.join("artifacts").to_string_lossy().into_owned(),
        grpc_listen_addr: None,
        github_webhook_secret: None,
        github_polling_enabled: false,
        github_poll_interval_secs: 30,
    };
    let mut state = crate::server::AppState::new(config, None);
    state.orchestration = orchestration;
    state.job_events = job_events;
    Ok(state)
}

async fn wait_for_running_job(
    service: &OrchestrationService,
    work_item_id: Uuid,
    worker_log: &Path,
    worker: &mut Child,
) -> anyhow::Result<Job> {
    let deadline = Instant::now() + E2E_TIMEOUT;
    loop {
        if let Some(status) = worker.try_wait()? {
            anyhow::bail!(
                "worker exited before running job with {status}: {}",
                std::fs::read_to_string(worker_log)
                    .unwrap_or_else(|error| format!("<unreadable log: {error}>"))
            );
        }
        if let Some(job) = service
            .list_jobs_for_work_item(work_item_id)
            .await?
            .into_iter()
            .find(|job| job.state == JobState::Running)
        {
            return Ok(job);
        }
        ensure!(Instant::now() < deadline, "running-job timeout");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_terminal_cancellation(
    service: &OrchestrationService,
    work_item_id: Uuid,
    job_id: Uuid,
    worker_log: &Path,
    worker: &mut Child,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + E2E_TIMEOUT;
    loop {
        if let Some(status) = worker.try_wait()? {
            anyhow::bail!(
                "worker exited before cancellation acknowledgement with {status}: {}",
                std::fs::read_to_string(worker_log)
                    .unwrap_or_else(|error| format!("<unreadable log: {error}>"))
            );
        }
        let job = service.get_job(job_id).await?.context("job disappeared")?;
        let item = service
            .get_work_item(work_item_id)
            .await?
            .context("work item disappeared")?;
        if job.state == JobState::Cancelled && item.state == WorkItemState::Cancelled {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "terminal cancellation timeout: job={:?} item={:?}",
            job.state,
            item.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_owned_container(worker_id: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + E2E_TIMEOUT;
    loop {
        if !owned_container_ids(worker_id)?.is_empty() {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "owned container did not appear");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn owned_container_ids(worker_id: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.reddit.agent-hub.worker={worker_id}"),
        ])
        .output()?;
    ensure!(output.status.success(), "docker ps failed");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect())
}

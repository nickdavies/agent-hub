use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use clap::{Parser, ValueEnum};
#[cfg(test)]
use protocol::canonical_runtime_spec_for_job;
use protocol::{
    AgentOutput, ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming,
    JOB_RUN_SCHEMA_NAME, Job, JobEvent, JobEventKind, JobResult, JobState, NetworkResourceKind,
    NetworkResourceRequest, RegisterWorkerRequest, Secret, TerminationReason, WorkerState,
    validate_agent_output_next_event,
};
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::watch;

mod artifacts;
mod client;
mod credential_provider;
mod egress;
mod egress_config;
mod mcp_bridge;
mod pb_conv;
mod workspace;

use agent_hub_config::RunnerProfilesConfig;
use artifacts::{
    AdditionalRepoMountView, AgentContextWriteParams, collect_structured_transcript_artifacts,
    display_paths_for_runtime, file_len_u64, normalize_github_repo_slug_with_allowlist,
    parse_github_pr_from_output, parse_github_repo_url, work_item_contract_from_job_context,
    workflow_valid_next_events_from_job_context, write_agent_context_file,
};
use client::{
    Client, GrpcClient, HttpClient, JobUpdateOutcome, is_worker_not_found_error,
    start_grpc_event_stream,
};
use credential_provider::{ResolvedCredentials, resolve_all};
use egress::{EgressManager, llm_provider_from_env};
use egress_config::{LocalAgentConfig, ManagedRunnerKind, load_local_agent_config};
use mcp_bridge::{McpBridgeInstance, container_socket_path, mcp_socket_env_var};
use pb_conv::ts_to_pb;
#[cfg(test)]
use pb_conv::{job_from_pb, job_to_pb};
use workspace::*;

pub mod pb {
    tonic::include_proto!("worker");
}

#[derive(Parser)]
#[command(name = "local-agent", version)]
struct Cli {
    #[arg(
        long,
        env = "AGENT_HUB_SERVER",
        default_value = "http://localhost:8080"
    )]
    server: String,

    #[arg(long, env = "AGENT_HUB_TOKEN")]
    token: Option<String>,

    #[arg(long)]
    worker_id: String,

    #[arg(long, default_value = "localhost")]
    hostname: String,

    #[arg(long, default_value = "./local-agent-data")]
    data_dir: PathBuf,

    #[arg(long, env = "ARTIFACT_STORE_ROOT")]
    artifact_store_root: Option<PathBuf>,

    /// Optional local-agent JSON config override (default: $CONFIG_ROOT/local-agent.json)
    #[arg(long)]
    local_agent_config: Option<PathBuf>,

    /// Override default agent runner (names a managed_runners entry).
    #[arg(long, env = "AGENT_HUB_DEFAULT_AGENT")]
    default_agent: Option<String>,

    #[arg(long, default_value_t = 3)]
    poll_interval_secs: u64,

    #[arg(long, default_value_t = 168)]
    transcript_retention_hours: u64,

    #[arg(long, value_enum, default_value_t = TransportKind::Http)]
    transport: TransportKind,

    /// gRPC endpoint, e.g. http://127.0.0.1:50051
    #[arg(long, env = "AGENT_HUB_GRPC", default_value = "http://127.0.0.1:50051")]
    grpc_endpoint: String,
}

#[derive(Clone, ValueEnum)]
enum TransportKind {
    Http,
    Grpc,
}

#[derive(Clone)]
struct JobEventPublisher {
    worker_id: String,
    tx: mpsc::Sender<pb::JobEventChunk>,
}

#[derive(Clone)]
struct JobEventSink {
    worker_id: String,
    writer: Arc<Mutex<JobEventTranscriptWriter>>,
    publisher: Option<JobEventPublisher>,
}

struct JobEventTranscriptWriter {
    file: tokio::fs::File,
}

impl JobEventPublisher {
    async fn emit(
        &self,
        job_id: uuid::Uuid,
        at: chrono::DateTime<Utc>,
        kind: pb::JobEventKind,
        message: String,
    ) {
        let _ = self
            .tx
            .send(pb::JobEventChunk {
                worker_id: self.worker_id.clone(),
                job_id: job_id.to_string(),
                at: Some(ts_to_pb(at)),
                kind: kind as i32,
                stream: job_event_stream_name(kind).map(ToOwned::to_owned),
                message,
            })
            .await;
    }
}

impl JobEventSink {
    async fn emit(&self, job_id: uuid::Uuid, kind: pb::JobEventKind, message: String) {
        let at = Utc::now();
        let event = JobEvent {
            job_id,
            worker_id: self.worker_id.clone(),
            at,
            kind: protocol_job_event_kind(kind),
            stream: job_event_stream_name(kind).map(ToOwned::to_owned),
            message: message.clone(),
        };
        {
            let mut writer = self.writer.lock().await;
            if let Err(error) = writer.write_event(&event).await {
                eprintln!(
                    "failed to append local job event transcript for {}: {error:#}",
                    event.job_id
                );
            }
        }

        if let Some(publisher) = &self.publisher {
            publisher.emit(job_id, at, kind, message).await;
        }
    }
}

impl JobEventTranscriptWriter {
    async fn create(path: &Path) -> anyhow::Result<Self> {
        let file = tokio::fs::File::create(path)
            .await
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self { file })
    }

    async fn write_event(&mut self, event: &JobEvent) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        self.file.write_all(&line).await?;
        self.file.flush().await?;
        Ok(())
    }
}

fn protocol_job_event_kind(kind: pb::JobEventKind) -> JobEventKind {
    pb_conv::protocol_job_event_kind(kind)
}

fn job_event_stream_name(kind: pb::JobEventKind) -> Option<&'static str> {
    match kind {
        pb::JobEventKind::JobEventStdout => Some("stdout"),
        pb::JobEventKind::JobEventStderr => Some("stderr"),
        _ => None,
    }
}

fn resolve_to_absolute(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn expand_home_path(path: &str) -> anyhow::Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed == "~" {
        let home = std::env::var("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home));
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = std::env::var("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home).join(rest));
    }

    Ok(PathBuf::from(trimmed))
}

enum RepoSourceKind {
    LocalDirectory(String),
    GitRemote(String),
}

fn classify_repo_source(entry: &str) -> RepoSourceKind {
    let trimmed = entry.trim();
    if trimmed.starts_with('/') || trimmed.starts_with('~') || trimmed.starts_with('.') {
        RepoSourceKind::LocalDirectory(trimmed.to_string())
    } else {
        RepoSourceKind::GitRemote(trimmed.to_string())
    }
}

fn canonical_approved_repository_roots(
    local_agent_config: &LocalAgentConfig,
) -> anyhow::Result<Vec<PathBuf>> {
    if local_agent_config.approved_repository_roots.is_empty() {
        anyhow::bail!(
            "assignment-driven triage requires explicit approved_repository_roots in local-agent config"
        );
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(std::fs::canonicalize)
        .transpose()
        .context("canonicalize HOME")?;
    let mut roots = Vec::new();
    for configured_root in &local_agent_config.approved_repository_roots {
        let absolute = resolve_to_absolute(configured_root).with_context(|| {
            format!(
                "resolve approved repository root {}",
                configured_root.display()
            )
        })?;
        let canonical = std::fs::canonicalize(&absolute).with_context(|| {
            format!(
                "canonicalize approved repository root {}",
                absolute.display()
            )
        })?;
        if !canonical.is_dir() {
            anyhow::bail!(
                "approved repository root '{}' is not a directory",
                canonical.display()
            );
        }
        if canonical == Path::new("/") || home.as_ref() == Some(&canonical) {
            anyhow::bail!(
                "approved repository root '{}' is too broad",
                canonical.display()
            );
        }
        if path_contains_credential_directory(&canonical) {
            anyhow::bail!(
                "approved repository root '{}' is credential-bearing",
                canonical.display()
            );
        }
        if roots.iter().any(|existing: &PathBuf| {
            canonical.starts_with(existing) || existing.starts_with(&canonical)
        }) {
            anyhow::bail!(
                "approved repository root '{}' duplicates or overlaps another root",
                canonical.display()
            );
        }
        roots.push(canonical);
    }
    Ok(roots)
}

fn path_contains_credential_directory(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                ".ssh" | ".aws" | ".gnupg" | ".kube" | ".docker" | ".config"
            )
        })
    })
}

fn validate_local_repo_dir(raw_dir: &str, approved_roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let expanded = expand_home_path(raw_dir)
        .with_context(|| format!("expand triage_repos entry '{raw_dir}'"))?;
    let host_dir = resolve_to_absolute(&expanded).with_context(|| {
        format!(
            "resolve triage_repos entry '{}' ({})",
            raw_dir,
            expanded.display()
        )
    })?;

    let host_dir = std::fs::canonicalize(&host_dir).with_context(|| {
        format!(
            "canonicalize triage_repos entry '{}' resolved as '{}'",
            raw_dir,
            host_dir.display()
        )
    })?;
    if !host_dir.is_dir() {
        anyhow::bail!(
            "triage_repos entry '{}' resolved to '{}' but is not a directory",
            raw_dir,
            host_dir.display()
        );
    }

    if !approved_roots.iter().any(|root| host_dir.starts_with(root)) {
        anyhow::bail!(
            "triage_repos entry '{}' canonicalized to '{}' outside approved_repository_roots",
            raw_dir,
            host_dir.display()
        );
    }
    if path_contains_credential_directory(&host_dir) {
        anyhow::bail!(
            "triage_repos entry '{}' canonicalized to credential-bearing path '{}'",
            raw_dir,
            host_dir.display()
        );
    }

    std::fs::read_dir(&host_dir).with_context(|| {
        format!(
            "triage_repos entry '{}' resolved to '{}' is not readable",
            raw_dir,
            host_dir.display()
        )
    })?;

    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&host_dir)
        .output()
        .with_context(|| format!("validate git repository at {}", host_dir.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "triage_repos entry '{}' canonicalized to '{}' but is not a valid git repository",
            raw_dir,
            host_dir.display()
        );
    }
    let top_level = String::from_utf8(output.stdout).context("git repository path is not UTF-8")?;
    let top_level =
        std::fs::canonicalize(top_level.trim()).context("canonicalize git repository top level")?;
    if top_level != host_dir {
        anyhow::bail!(
            "triage_repos entry '{}' must name a repository root, not '{}' within '{}'",
            raw_dir,
            host_dir.display(),
            top_level.display()
        );
    }

    Ok(host_dir)
}

fn spawn_repo_cache_fetcher(
    remote_repos: Vec<String>,
    cache_dir: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            for url in &remote_repos {
                let name = repo_name_from_url(url);
                let repo_dir = cache_dir.join(&name);
                match std::process::Command::new("git")
                    .args(["fetch", "--all"])
                    .current_dir(&repo_dir)
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        eprintln!(
                            "warning: background fetch failed for {url}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    Err(error) => {
                        eprintln!("warning: background fetch failed for {url}: {error}");
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

fn validate_triage_repos_and_prime_cache(
    local_agent_config: &LocalAgentConfig,
    repo_cache_dir: &Path,
) -> anyhow::Result<Vec<String>> {
    if local_agent_config.triage_repos.is_empty() {
        anyhow::bail!(
            "assignment-driven triage requires at least one pre-materialized triage_repos directory"
        );
    }
    let approved_roots = canonical_approved_repository_roots(local_agent_config)?;
    std::fs::create_dir_all(repo_cache_dir)
        .with_context(|| format!("create repo cache directory {}", repo_cache_dir.display()))?;

    let remote_repos = Vec::new();
    for raw_entry in &local_agent_config.triage_repos {
        match classify_repo_source(raw_entry) {
            RepoSourceKind::LocalDirectory(raw_dir) => {
                validate_local_repo_dir(&raw_dir, &approved_roots)?;
            }
            RepoSourceKind::GitRemote(url) => {
                anyhow::bail!(
                    "triage_repos entry '{url}' is remote; triage requires pre-materialized local directories"
                );
            }
        }
    }

    Ok(remote_repos)
}

#[cfg(test)]
fn configured_repo_discovery_mounts(
    local_agent_config: &LocalAgentConfig,
    repo_cache_dir: &Path,
) -> anyhow::Result<Vec<protocol::orchestration::PlannedFileMount>> {
    let mut mounts = Vec::new();
    let approved_roots = canonical_approved_repository_roots(local_agent_config)?;
    let mut canonical_sources = Vec::new();
    let mut aliases = HashSet::new();

    for raw_entry in &local_agent_config.triage_repos {
        let host_dir = match classify_repo_source(raw_entry) {
            RepoSourceKind::LocalDirectory(raw_dir) => {
                validate_local_repo_dir(&raw_dir, &approved_roots)?
            }
            RepoSourceKind::GitRemote(url) => {
                let repo_dir = repo_cache_dir.join(repo_name_from_url(&url));
                if !repo_dir.exists() {
                    anyhow::bail!(
                        "triage_repos entry '{}' expected cached repo at '{}' but it does not exist",
                        url,
                        repo_dir.display()
                    );
                }
                if !repo_dir.is_dir() {
                    anyhow::bail!(
                        "triage_repos entry '{}' expected cached repo at '{}' but it is not a directory",
                        url,
                        repo_dir.display()
                    );
                }
                if !repo_dir.join(".git").is_dir() {
                    anyhow::bail!(
                        "triage_repos entry '{}' expected cached repo at '{}' but it is not a git repository",
                        url,
                        repo_dir.display()
                    );
                }
                repo_dir
            }
        };

        let dirname = host_dir
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("repos");
        if !dirname
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            || matches!(dirname, "." | "..")
        {
            anyhow::bail!(
                "triage_repos source '{}' has unsafe mount alias '{}'",
                host_dir.display(),
                dirname
            );
        }
        if !aliases.insert(dirname.to_string()) {
            anyhow::bail!("triage_repos contains duplicate mount alias '/repos/{dirname}'");
        }
        if canonical_sources
            .iter()
            .any(|source: &PathBuf| host_dir.starts_with(source) || source.starts_with(&host_dir))
        {
            anyhow::bail!(
                "triage_repos source '{}' duplicates or overlaps another source",
                host_dir.display()
            );
        }
        canonical_sources.push(host_dir.clone());
        mounts.push(protocol::orchestration::PlannedFileMount {
            host_path: host_dir.to_string_lossy().into_owned(),
            container_path: format!("/repos/{dirname}"),
            read_only: true,
        });
    }

    Ok(mounts)
}

fn approval_mode_from_job_context(context: &serde_json::Value) -> &'static str {
    if context
        .get("approval_mode")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("gateway"))
    {
        "gateway"
    } else {
        "auto"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::{inline_job_events_json_tail, inline_transcript_jsonl_tail};

    #[tokio::test]
    async fn heartbeat_shutdown_watch_signal_is_sticky() {
        let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::watch::channel(false);

        hb_stop_tx
            .send(true)
            .expect("watch receiver should accept shutdown signal");

        let changed = tokio::time::timeout(Duration::from_millis(50), hb_stop_rx.changed())
            .await
            .expect("shutdown signal should already be visible");
        assert!(changed.is_ok(), "watch channel should remain open");
        assert!(*hb_stop_rx.borrow(), "shutdown value should be true");
    }

    #[test]
    fn grpc_job_mapping_roundtrip() {
        let now = Utc::now();
        let mut env = std::collections::HashMap::new();
        env.insert("K".to_string(), "V".to_string());
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step1".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec!["hi".into()],
            env,
            prompt: Some(protocol::PromptRef {
                system: None,
                user: "u".into(),
            }),
            context: serde_json::json!({"x":1}),
            repo: Some(protocol::RepoSource {
                repo_url: "https://example/repo.git".into(),
                base_ref: "origin/main".into(),
                branch_name: Some("agent-hub/abc".into()),
            }),
            timeout_secs: Some(10),
            max_retries: 2,
            attempt: 1,
            transcript_dir: Some("/tmp/t".into()),
            result: None,
            lease_expires_at: Some(now),
            created_at: now,
            updated_at: now,
        };

        let pb = job_to_pb(job.clone());
        let decoded = job_from_pb(pb).expect("decode");
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.execution_mode, job.execution_mode);
        assert_eq!(decoded.max_retries, 2);
    }

    #[test]
    fn worker_rejects_mutated_authorization_before_execution() {
        let now = Utc::now();
        let work_item_id = uuid::Uuid::new_v4();
        let mut job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id,
            step_id: "build".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: Some("worker".to_string()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("image@sha256:abc".to_string()),
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
            env: HashMap::from([("MODE".to_string(), "test".to_string())]),
            prompt: Some(protocol::PromptRef {
                system: Some("system".to_string()),
                user: "user".to_string(),
            }),
            context: serde_json::json!({
                "workflow": {"inputs":{"scope":"signed"}, "valid_next_events":["job_succeeded"]},
                "work_item": {"workflow_id":"workflow"},
                "state_invocation": {"state_id":"build", "id":work_item_id}
            }),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/example/repo.git".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            timeout_secs: Some(60),
            max_retries: 3,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let environment = protocol::canonical_environment_for_execution(
            ExecutionMode::Docker,
            job.container_image.clone(),
            job.timeout_secs,
        );
        let request = protocol::ResolvedAuthorizationRequest {
            version: protocol::AUTHORIZATION_REQUEST_VERSION.to_string(),
            invocation_id: work_item_id,
            work_unit_id: work_item_id,
            root_work_item_id: work_item_id,
            workflow_id: "workflow".to_string(),
            workflow_snapshot: serde_json::json!({"schema":"reactive_v2"}),
            state_id: job.step_id.clone(),
            state_kind: protocol::ReactiveStateKind::Command,
            behavior: protocol::ResolvedAuthorizationBehavior {
                execution_kind: "command".to_string(),
                runner_profile: serde_json::json!({
                    "profile_name":"command",
                    "config_digest":"command_v1"
                }),
                command: job.command.clone(),
                args: job.args.clone(),
                env: job.env.clone().into_iter().collect(),
                prompt: job.prompt.clone(),
                primary_repo: job.repo.clone(),
                primary_repo_branch: Some(protocol::authorized_default_branch(
                    work_item_id,
                    work_item_id,
                )),
                execution_context: protocol::authorization_execution_context(&job.context),
                triage: false,
                allow_remote_credentials: true,
            },
            permission_envelope: protocol::CanonicalRuntimeSpec {
                runtime_version: "v2".to_string(),
                permissions: protocol::canonical_permissions_for_environment(&environment),
                environment,
                approval_rules: None,
                repos: Vec::new(),
                github_capabilities: Vec::new(),
            },
            resolved_environment: serde_json::json!({}),
            provider: protocol::ResolvedAuthorizationProvider {
                provider: Some("test".to_string()),
                task_id: Some("task".to_string()),
            },
            resource_constraints: protocol::ResolvedAuthorizationResources {
                timeout_secs: Some(60),
                max_retries: 3,
            },
        };
        protocol::project_authorization_request_into_job_context(
            &mut job.context,
            "authorization",
            &request,
        )
        .expect("authorization evidence");
        assert!(verify_authorized_job(&job, &LocalAgentConfig::default()).is_ok());
        let http_roundtrip: Job =
            serde_json::from_value(serde_json::to_value(&job).expect("HTTP job JSON"))
                .expect("HTTP authorization evidence");
        let http_verified = verify_authorized_job(&http_roundtrip, &LocalAgentConfig::default())
            .expect("HTTP verify");
        let transported = job_from_pb(job_to_pb(job.clone())).expect("gRPC authorization evidence");
        let grpc_verified =
            verify_authorized_job(&transported, &LocalAgentConfig::default()).expect("gRPC verify");
        let expected_branch = protocol::authorized_default_branch(work_item_id, work_item_id);
        for verified in [&http_verified, &grpc_verified] {
            assert_eq!(
                resolve_target_branch_for_repo_job(
                    &verified.job,
                    verified.job.repo.as_ref().expect("authorized repo"),
                ),
                expected_branch
            );
            let transported_request =
                protocol::authorization_request_from_job_context(&verified.job.context)
                    .expect("transported request");
            assert_eq!(
                transported_request
                    .request_digest()
                    .expect("transported digest"),
                request.request_digest().expect("source digest")
            );
            assert_eq!(transported_request.resource_constraints.max_retries, 3);
        }

        let mut http_attempt_mutation = job.clone();
        http_attempt_mutation.attempt = 99;
        let http_attempt_mutation: Job = serde_json::from_value(
            serde_json::to_value(http_attempt_mutation).expect("HTTP attempt mutation"),
        )
        .expect("HTTP mutated job");
        let http_attempt_verified =
            verify_authorized_job(&http_attempt_mutation, &LocalAgentConfig::default())
                .expect("HTTP attempt is host-only");
        assert_eq!(
            resolve_target_branch_for_repo_job(
                &http_attempt_verified.job,
                http_attempt_verified.job.repo.as_ref().expect("HTTP repo"),
            ),
            expected_branch
        );

        let mut grpc_attempt_mutation = job.clone();
        grpc_attempt_mutation.attempt = 77;
        let grpc_attempt_mutation =
            job_from_pb(job_to_pb(grpc_attempt_mutation)).expect("gRPC attempt mutation");
        let grpc_attempt_verified =
            verify_authorized_job(&grpc_attempt_mutation, &LocalAgentConfig::default())
                .expect("gRPC attempt is host-only");
        assert_eq!(
            resolve_target_branch_for_repo_job(
                &grpc_attempt_verified.job,
                grpc_attempt_verified.job.repo.as_ref().expect("gRPC repo"),
            ),
            expected_branch
        );

        for mutated in [
            {
                let mut mutated = job.clone();
                mutated
                    .env
                    .insert("MODE".to_string(), "mutated".to_string());
                mutated
            },
            {
                let mut mutated = job.clone();
                mutated.prompt.as_mut().unwrap().user = "mutated".to_string();
                mutated
            },
            {
                let mut mutated = job.clone();
                mutated.repo.as_mut().unwrap().base_ref = "origin/other".to_string();
                mutated
            },
            {
                let mut mutated = job.clone();
                mutated.context["workflow"]["inputs"]["scope"] =
                    serde_json::Value::String("mutated".to_string());
                mutated
            },
        ] {
            assert!(verify_authorized_job(&mutated, &LocalAgentConfig::default()).is_err());
        }

        let mut profile_mismatch = job.clone();
        let mut mismatched_request = request.clone();
        mismatched_request.behavior.runner_profile["config_digest"] =
            serde_json::Value::String("other".to_string());
        protocol::project_authorization_request_into_job_context(
            &mut profile_mismatch.context,
            "authorization",
            &mismatched_request,
        )
        .expect("profile mismatch evidence");
        assert!(verify_authorized_job(&profile_mismatch, &LocalAgentConfig::default()).is_err());

        let mut environment_mismatch = job.clone();
        let mut mismatched_request = request.clone();
        mismatched_request.resolved_environment = serde_json::json!({"base":"missing"});
        protocol::project_authorization_request_into_job_context(
            &mut environment_mismatch.context,
            "authorization",
            &mismatched_request,
        )
        .expect("environment mismatch evidence");
        assert!(
            verify_authorized_job(&environment_mismatch, &LocalAgentConfig::default()).is_err()
        );

        job.context[protocol::CONTEXT_REQUEST_DIGEST_KEY] =
            serde_json::Value::String("mutated".to_string());
        assert!(verify_authorized_job(&job, &LocalAgentConfig::default()).is_err());
    }

    #[test]
    fn parse_github_https_repo_slug() {
        let allowlist = LocalAgentConfig::default().github_host_allowlist();
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "https://github.com/reddit/agent-hub.git",
                &allowlist,
            ),
            Some("reddit/agent-hub".to_string())
        );
    }

    #[test]
    fn parse_github_ssh_repo_slug_is_rejected() {
        let allowlist = LocalAgentConfig::default().github_host_allowlist();
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "git@github.com:reddit/agent-hub.git",
                &allowlist,
            ),
            None
        );
    }

    #[test]
    fn parse_github_repo_url_extracts_host_and_slug_for_https() {
        let parsed = parse_github_repo_url("https://github.snooguts.net/reddit/agent-hub.git")
            .expect("parse https GHE URL");
        assert_eq!(parsed.host, "github.snooguts.net");
        assert_eq!(parsed.slug, "reddit/agent-hub");
    }

    #[test]
    fn parse_github_repo_url_rejects_ssh_and_adversarial_urls() {
        for value in [
            "git@github.snooguts.net:reddit/agent-hub.git",
            "http://github.com/reddit/agent-hub",
            "https://user@github.com/reddit/agent-hub",
            "https://github.com:443/reddit/agent-hub",
            "https://github.com/reddit/agent-hub/extra",
            "https://github.com/reddit/%2e%2e",
            "https://github.com/reddit/agent-hub?x=1",
            "https://github.com/reddit/agent-hub#fragment",
            "https://github.com/reddit/bad repo",
        ] {
            assert!(parse_github_repo_url(value).is_none(), "accepted {value}");
        }
    }

    #[test]
    fn parse_github_repo_url_rejects_non_github_shaped_urls() {
        assert!(parse_github_repo_url("https://gitlab.com/foo/bar").is_some()); // shape-only
        assert!(parse_github_repo_url("not-a-url").is_none());
        assert!(parse_github_repo_url("https://example.com/onlyone").is_none());
        assert!(parse_github_repo_url("https:///missing/host").is_none());
    }

    #[test]
    fn local_agent_config_default_allowlist_only_accepts_github_com() {
        let cfg = LocalAgentConfig::default();
        let allowlist = cfg.github_host_allowlist();
        assert_eq!(allowlist, vec!["github.com".to_string()]);
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "https://github.snooguts.net/reddit/agent-hub.git",
                &allowlist,
            ),
            None,
            "GHE host should be rejected by default config",
        );
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "https://github.com/reddit/agent-hub.git",
                &allowlist,
            ),
            Some("reddit/agent-hub".to_string()),
        );
    }

    #[test]
    fn local_agent_config_github_hosts_allowlists_extra_entries() {
        let mut cfg = LocalAgentConfig::default();
        cfg.github_hosts = vec!["github.com".to_string(), "github.snooguts.net".to_string()];
        let allowlist = cfg.github_host_allowlist();
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "https://github.snooguts.net/reddit/agent-hub.git",
                &allowlist,
            ),
            Some("reddit/agent-hub".to_string()),
        );
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "https://github.com/reddit/agent-hub.git",
                &allowlist,
            ),
            Some("reddit/agent-hub".to_string()),
        );
    }

    #[test]
    fn local_agent_config_github_hosts_blank_entries_fall_back_to_default() {
        let mut cfg = LocalAgentConfig::default();
        cfg.github_hosts = vec!["   ".to_string(), "".to_string()];
        assert_eq!(
            cfg.github_host_allowlist(),
            vec!["github.com".to_string()],
            "blank entries should be ignored and fall back to the default",
        );
    }

    #[test]
    fn triage_repo_validation_requires_pre_materialized_local_inputs() {
        let tmp = std::env::temp_dir().join(format!("triage-local-{}", uuid::Uuid::new_v4()));
        let approved = tmp.join("approved");
        let repo = approved.join("repo");
        std::fs::create_dir_all(&repo).expect("create local repo input");
        let init = std::process::Command::new("git")
            .arg("init")
            .arg(&repo)
            .output()
            .expect("run git init");
        assert!(init.status.success());
        let cache = tmp.join("cache");

        let empty = LocalAgentConfig::default();
        assert!(validate_triage_repos_and_prime_cache(&empty, &cache).is_err());

        let mut remote = LocalAgentConfig::default();
        remote.triage_repos = vec!["https://github.com/reddit/agent-hub.git".to_string()];
        remote.approved_repository_roots = vec![approved.clone()];
        assert!(validate_triage_repos_and_prime_cache(&remote, &cache).is_err());

        let mut local = LocalAgentConfig::default();
        local.triage_repos = vec![repo.to_string_lossy().into_owned()];
        local.approved_repository_roots = vec![approved];
        assert!(
            validate_triage_repos_and_prime_cache(&local, &cache)
                .expect("local pre-materialized repo")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn triage_repo_mounts_reject_unapproved_sensitive_non_repo_and_alias_collisions() {
        let tmp = std::env::temp_dir().join(format!("triage-mounts-{}", uuid::Uuid::new_v4()));
        let approved = tmp.join("approved");
        let repo = approved.join("same-name");
        let other_parent = tmp.join("other");
        let other_repo = other_parent.join("same-name");
        let non_repo = approved.join("not-a-repo");
        for path in [&repo, &other_repo, &non_repo] {
            std::fs::create_dir_all(path).expect("create test directory");
        }
        for path in [&repo, &other_repo] {
            let init = std::process::Command::new("git")
                .arg("init")
                .arg(path)
                .output()
                .expect("run git init");
            assert!(init.status.success());
        }

        let mut config = LocalAgentConfig::default();
        config.approved_repository_roots = vec![approved.clone()];

        config.triage_repos = vec![other_repo.to_string_lossy().into_owned()];
        assert!(configured_repo_discovery_mounts(&config, &tmp.join("cache")).is_err());

        config.triage_repos = vec![non_repo.to_string_lossy().into_owned()];
        assert!(configured_repo_discovery_mounts(&config, &tmp.join("cache")).is_err());

        config.approved_repository_roots = vec![PathBuf::from("/etc")];
        config.triage_repos = vec!["/etc".to_string()];
        assert!(configured_repo_discovery_mounts(&config, &tmp.join("cache")).is_err());

        config.approved_repository_roots = vec![approved.clone(), other_parent.clone()];
        config.triage_repos = vec![
            repo.to_string_lossy().into_owned(),
            other_repo.to_string_lossy().into_owned(),
        ];
        assert!(configured_repo_discovery_mounts(&config, &tmp.join("cache")).is_err());

        let credential_root = tmp.join(".ssh");
        std::fs::create_dir_all(&credential_root).expect("create credential directory");
        config.approved_repository_roots = vec![credential_root];
        config.triage_repos.clear();
        assert!(canonical_approved_repository_roots(&config).is_err());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[cfg(unix)]
    #[test]
    fn triage_repo_mounts_reject_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("triage-symlink-{}", uuid::Uuid::new_v4()));
        let approved = tmp.join("approved");
        let outside = tmp.join("outside-repo");
        std::fs::create_dir_all(&approved).expect("create approved root");
        let init = std::process::Command::new("git")
            .arg("init")
            .arg(&outside)
            .output()
            .expect("run git init");
        assert!(init.status.success());
        let escaped = approved.join("escaped");
        symlink(&outside, &escaped).expect("create symlink");

        let mut config = LocalAgentConfig::default();
        config.approved_repository_roots = vec![approved];
        config.triage_repos = vec![escaped.to_string_lossy().into_owned()];
        assert!(configured_repo_discovery_mounts(&config, &tmp.join("cache")).is_err());

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn parse_github_pr_contract_from_output_json() {
        let parsed = parse_github_pr_from_output(&serde_json::json!({
            "github_pr": {
                "number": 123,
                "url": "https://github.com/reddit/agent-hub/pull/123",
                "state": "open",
                "is_draft": false,
                "head_ref_name": "agent-hub/test",
                "base_ref_name": "main"
            }
        }))
        .expect("should parse github_pr contract");
        assert_eq!(parsed.number, 123);
        assert_eq!(parsed.state.as_deref(), Some("open"));
        assert_eq!(parsed.base_ref_name.as_deref(), Some("main"));
    }

    #[test]
    fn collect_structured_transcript_artifacts_marks_agent_context_and_optional_jsonl() {
        let root =
            std::env::temp_dir().join(format!("structured-artifacts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        std::fs::write(&agent_context_path, "{\"schema\":\"agent_context_v1\"}\n")
            .expect("write agent context");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].key, "agent_context");
        assert!(artifacts[0].bytes.is_some());
        assert_eq!(artifacts[0].schema.as_deref(), Some("agent_context_v1"));
        assert_eq!(artifacts[0].record_count, None);
        assert_eq!(
            artifacts[0]
                .inline_json
                .as_ref()
                .and_then(|v| v.get("schema"))
                .and_then(serde_json::Value::as_str),
            Some("agent_context_v1")
        );

        std::fs::write(&transcript_jsonl_path, "{\"event\":\"x\"}\n").expect("write jsonl");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 2);
        let agent_context = artifacts
            .iter()
            .find(|a| a.key == "agent_context")
            .expect("agent_context artifact");
        assert_eq!(agent_context.schema.as_deref(), Some("agent_context_v1"));
        assert_eq!(agent_context.record_count, None);

        let transcript_jsonl = artifacts
            .iter()
            .find(|a| a.key == "transcript_jsonl")
            .expect("transcript_jsonl artifact");
        assert_eq!(transcript_jsonl.schema, None);
        assert_eq!(transcript_jsonl.record_count, Some(1));
        let inline = transcript_jsonl
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("transcript_jsonl inline tail array");
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].as_str(), Some("{\"event\":\"x\"}"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_skips_inline_for_unknown_agent_context_schema() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-schema-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        std::fs::write(&agent_context_path, "{\"schema\":\"agent_context_v2\"}\n")
            .expect("write agent context");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].key, "agent_context");
        assert!(artifacts[0].inline_json.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_includes_job_events_bounded_inline_tail() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        let first_job_id = uuid::Uuid::new_v4();
        let second_job_id = uuid::Uuid::new_v4();
        let payload = format!(
            "{{\"job_id\":\"{first_job_id}\",\"worker_id\":\"w1\",\"at\":\"{}\",\"kind\":\"system\",\"message\":\"first\"}}\n{{\"job_id\":\"{second_job_id}\",\"worker_id\":\"w1\",\"at\":\"{}\",\"kind\":\"stdout\",\"stream\":\"stdout\",\"message\":\"second\"}}\n",
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        let job_events = artifacts.first().expect("job_events artifact");
        assert_eq!(job_events.key, "job_events");
        assert_eq!(
            job_events.schema.as_deref(),
            Some(protocol::JOB_EVENT_SCHEMA_NAME)
        );
        assert_eq!(job_events.bytes, file_len_u64(&job_events_path));
        assert_eq!(job_events.record_count, Some(2));
        let inline = job_events
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("job_events inline array");
        assert_eq!(inline.len(), 2);
        assert_eq!(
            inline[0].get("job_id").and_then(serde_json::Value::as_str),
            Some(first_job_id.to_string().as_str())
        );
        assert_eq!(
            inline[1].get("kind").and_then(serde_json::Value::as_str),
            Some("stdout")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_job_events_inline_tail_skips_malformed_lines() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-malformed-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        let valid_event_a = serde_json::json!({
            "job_id": uuid::Uuid::new_v4(),
            "worker_id": "w1",
            "at": Utc::now(),
            "kind": "system",
            "message": "ok-a"
        });
        let valid_event_b = serde_json::json!({
            "job_id": uuid::Uuid::new_v4(),
            "worker_id": "w1",
            "at": Utc::now(),
            "kind": "stderr",
            "stream": "stderr",
            "message": "ok-b"
        });
        let payload = format!(
            "{}\nnot-json\n{}\n{{\"job_id\":\"{}\"}}\n",
            serde_json::to_string(&valid_event_a).expect("serialize event a"),
            serde_json::to_string(&valid_event_b).expect("serialize event b"),
            uuid::Uuid::new_v4(),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        let job_events = artifacts.first().expect("job_events artifact");
        assert_eq!(job_events.record_count, Some(4));
        let inline = job_events
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("job_events inline array");
        assert_eq!(inline.len(), 2);
        assert_eq!(
            inline[0].get("message").and_then(serde_json::Value::as_str),
            Some("ok-a")
        );
        assert_eq!(
            inline[1].get("message").and_then(serde_json::Value::as_str),
            Some("ok-b")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inline_job_events_json_tail_respects_total_byte_budget() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-budget-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let job_events_path = root.join("job-events.jsonl");

        let mk_event = |message: &str| {
            serde_json::json!({
                "job_id": uuid::Uuid::new_v4(),
                "worker_id": "w1",
                "at": Utc::now(),
                "kind": "system",
                "message": message,
            })
        };
        let payload = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&mk_event("small-a")).expect("serialize a"),
            serde_json::to_string(&mk_event(&"x".repeat(2048))).expect("serialize large"),
            serde_json::to_string(&mk_event("small-b")).expect("serialize b"),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let inline = inline_job_events_json_tail(&job_events_path, 20, 512)
            .and_then(|v| v.as_array().cloned())
            .expect("inline tail array");
        assert_eq!(inline.len(), 1);
        assert_eq!(
            inline[0].get("message").and_then(serde_json::Value::as_str),
            Some("small-b")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inline_transcript_jsonl_tail_respects_record_and_byte_budgets() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-transcript-inline-budget-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let transcript_jsonl_path = root.join("transcript.jsonl");

        std::fs::write(
            &transcript_jsonl_path,
            format!("line-a\n{}\nline-b\nline-c\n", "x".repeat(2048)),
        )
        .expect("write transcript jsonl");

        let inline = inline_transcript_jsonl_tail(&transcript_jsonl_path, 2, 64)
            .and_then(|v| v.as_array().cloned())
            .expect("inline tail array");
        assert_eq!(inline.len(), 2);
        assert_eq!(inline[0].as_str(), Some("line-b"));
        assert_eq!(inline[1].as_str(), Some("line-c"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_includes_run_json_with_schema_gated_inline() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-run-json-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let run_json_path = root.join("run.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");

        std::fs::write(
            &run_json_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": protocol::JOB_RUN_SCHEMA_NAME,
                "job_id": uuid::Uuid::new_v4(),
                "workspace": "/workspace"
            }))
            .expect("serialize run payload"),
        )
        .expect("write run json");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        let run_json = artifacts.first().expect("run_json artifact");
        assert_eq!(run_json.key, "run_json");
        assert_eq!(
            run_json.schema.as_deref(),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(run_json.bytes, file_len_u64(&run_json_path));
        assert_eq!(
            run_json
                .inline_json
                .as_ref()
                .and_then(|v| v.get("schema"))
                .and_then(serde_json::Value::as_str),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );

        std::fs::write(
            &run_json_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": "job_run_v2",
                "job_id": uuid::Uuid::new_v4()
            }))
            .expect("serialize unknown run payload"),
        )
        .expect("overwrite run json");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        let run_json = artifacts.first().expect("run_json artifact");
        assert!(run_json.inline_json.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn supervise_process_wait_strategy_does_not_depend_on_second_command_spawn() {
        // Regression guard for no-timeout branch: command must execute once.
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root =
                std::env::temp_dir().join(format!("run-process-once-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("mkdir root");
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).expect("mkdir workspace");
            let transcript = root.join("transcript");
            std::fs::create_dir_all(&transcript).expect("mkdir transcript");

            let counter = workspace.join("count.txt");
            let script = format!(
                "if [ -f \"{}\" ]; then n=$(cat \"{}\"); else n=0; fi; n=$((n+1)); echo $n > \"{}\"",
                counter.display(),
                counter.display(),
                counter.display()
            );

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                command: "sh".into(),
                args: vec!["-lc".into(), script],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: None,
                timeout_secs: None,
                max_retries: 0,
                attempt: 0,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let status = supervise_process(
                &job,
                &workspace,
                &transcript.join("transcript.jsonl"),
                &transcript.join("stdout.log"),
                &transcript.join("stderr.log"),
                &std::collections::HashMap::new(),
                None,
                None,
                None,
            )
            .await
            .expect("supervise_process");

            assert!(matches!(status, ExecutionStatus::Exited(0)));
            let n = std::fs::read_to_string(&counter).expect("counter exists");
            assert_eq!(n.trim(), "1");

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn supervise_process_output_validation_stops_lingering_process() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!(
                "run-process-validate-early-exit-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).expect("mkdir root");
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).expect("mkdir workspace");
            let transcript = root.join("transcript");
            std::fs::create_dir_all(&transcript).expect("mkdir transcript");
            let output_path = transcript.join("output.json");

            let script = format!(
                "printf '%s' '{{\"next_event\":\"job_succeeded\"}}' > \"{}\"; sleep 10",
                output_path.display()
            );

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                command: "sh".into(),
                args: vec!["-lc".into(), script],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: None,
                timeout_secs: Some(30),
                max_retries: 0,
                attempt: 0,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let started = Instant::now();
            let status = supervise_process_with_output_validation(
                &job,
                &workspace,
                &transcript.join("transcript.jsonl"),
                &transcript.join("stdout.log"),
                &transcript.join("stderr.log"),
                &std::collections::HashMap::new(),
                job.timeout_secs,
                &output_path,
                &["job_succeeded".to_string()],
                &serde_json::json!({}),
                &EnvironmentValidationTable::for_tests(),
                None,
                None,
                false,
            )
            .await
            .expect("supervise_process_with_output_validation");

            assert!(matches!(status, ExecutionStatus::Exited(0)));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "expected early-exit before lingering sleep"
            );

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn supervise_process_output_validation_rejects_invalid_output() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!(
                "run-process-validate-invalid-output-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).expect("mkdir root");
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).expect("mkdir workspace");
            let transcript = root.join("transcript");
            std::fs::create_dir_all(&transcript).expect("mkdir transcript");
            let output_path = transcript.join("output.json");

            let script = format!(
                "printf '%s' '{{\"next_event\":\"not_allowed\"}}' > \"{}\"; sleep 5",
                output_path.display()
            );

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                command: "sh".into(),
                args: vec!["-lc".into(), script],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: None,
                timeout_secs: Some(1),
                max_retries: 0,
                attempt: 0,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let status = supervise_process_with_output_validation(
                &job,
                &workspace,
                &transcript.join("transcript.jsonl"),
                &transcript.join("stdout.log"),
                &transcript.join("stderr.log"),
                &std::collections::HashMap::new(),
                job.timeout_secs,
                &output_path,
                &["job_succeeded".to_string()],
                &serde_json::json!({}),
                &EnvironmentValidationTable::for_tests(),
                None,
                None,
                false,
            )
            .await
            .expect("supervise_process_with_output_validation");

            assert!(matches!(status, ExecutionStatus::TimedOut));

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn supervise_process_cancel_signal_kills_process() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root =
                std::env::temp_dir().join(format!("run-process-cancel-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("mkdir root");
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).expect("mkdir workspace");
            let transcript = root.join("transcript");
            std::fs::create_dir_all(&transcript).expect("mkdir transcript");

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                command: "sh".into(),
                args: vec!["-lc".into(), "sleep 30".into()],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: None,
                timeout_secs: Some(60),
                max_retries: 0,
                attempt: 0,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let (cancel_tx, cancel_rx) = watch::channel(false);
            let transcript_jsonl_path = transcript.join("transcript.jsonl");
            let stdout_path = transcript.join("stdout.log");
            let stderr_path = transcript.join("stderr.log");
            let env = std::collections::HashMap::new();

            let run = supervise_process(
                &job,
                &workspace,
                &transcript_jsonl_path,
                &stdout_path,
                &stderr_path,
                &env,
                Some(60),
                None,
                Some(cancel_rx),
            );

            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel_tx.send(true).expect("send cancel");

            let status = run.await.expect("supervise_process");
            assert!(matches!(status, ExecutionStatus::Cancelled));

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn branch_for_job_attempt_preserves_configured_branch() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 2,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let branch = branch_for_job_attempt(&job, Some("agent-hub/manual".to_string()));
        assert_eq!(branch, "agent-hub/manual");
    }

    #[test]
    fn branch_and_worktree_fallback_are_attempt_scoped() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 2,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let branch = branch_for_job_attempt(&job, None);
        assert!(branch.starts_with("agent-hub/"));
        assert!(branch.contains("-a2"));

        let root = std::env::temp_dir().join(format!("wt-root-{}", uuid::Uuid::new_v4()));
        let wt = worktree_dir_for_job_attempt(&root, &job);
        let wt_str = wt.to_string_lossy();
        assert!(wt_str.contains(&job.work_item_id.to_string()));
        assert!(wt_str.contains("-a2"));
    }

    #[test]
    fn preferred_checkout_ref_from_job_results_prefers_matching_base_ref_head_sha() {
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let context = serde_json::json!({
            "job_results": {
                "impl": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "agent-hub/feature-a",
                        "head_sha": "abc123"
                    }
                },
                "other": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/release",
                        "head_sha": "def456"
                    }
                }
            }
        });

        let selected =
            preferred_checkout_ref_from_job_results(&context, &repo, "agent-hub/feature-a");
        assert_eq!(selected.as_deref(), Some("abc123"));
    }

    #[test]
    fn preferred_checkout_ref_from_job_results_uses_latest_successful_matching_lineage() {
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let context = serde_json::json!({
            "job_results": {
                "triage": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "main",
                        "head_sha": "base-sha"
                    },
                    "timing": {
                        "finished_at": "2026-04-23T10:00:00Z"
                    }
                },
                "implement": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "agent-hub/feature-x",
                        "head_sha": "commit-a"
                    },
                    "timing": {
                        "finished_at": "2026-04-23T10:05:00Z"
                    }
                },
                "ai_review": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "agent-hub/feature-x",
                        "head_sha": "commit-b"
                    },
                    "timing": {
                        "finished_at": "2026-04-23T10:10:00Z"
                    }
                }
            }
        });

        let selected =
            preferred_checkout_ref_from_job_results(&context, &repo, "agent-hub/feature-x");
        assert_eq!(selected.as_deref(), Some("commit-b"));
    }

    #[test]
    fn preferred_checkout_ref_from_job_results_falls_back_to_matching_target_branch_when_head_sha_missing()
     {
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let context = serde_json::json!({
            "job_results": {
                "push_to_remote": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "agent-hub/feature-y"
                    }
                }
            }
        });

        let selected =
            preferred_checkout_ref_from_job_results(&context, &repo, "agent-hub/feature-y");
        assert_eq!(selected.as_deref(), Some("agent-hub/feature-y"));
    }

    #[test]
    fn checkout_ref_for_repo_job_falls_back_to_repo_base_ref() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({"job_results": {"impl": {"outcome": "failed"}}}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let selected = checkout_ref_for_repo_job(&job, &repo);
        assert_eq!(selected, "origin/main");
    }

    #[test]
    fn checkout_ref_for_repo_job_uses_latest_matching_branch_lineage_head_sha() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "push_to_remote".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({
                "job_results": {
                    "triage": {
                        "outcome": "succeeded",
                        "repo": {
                            "base_ref": "origin/main",
                            "target_branch": "main",
                            "head_sha": "base-sha"
                        },
                        "timing": {
                            "finished_at": "2026-04-23T10:00:00Z"
                        }
                    },
                    "implement": {
                        "outcome": "succeeded",
                        "repo": {
                            "base_ref": "origin/main",
                            "target_branch": "agent-hub/feature-x",
                            "head_sha": "commit-a"
                        },
                        "timing": {
                            "finished_at": "2026-04-23T10:05:00Z"
                        }
                    },
                    "ai_review": {
                        "outcome": "succeeded",
                        "repo": {
                            "base_ref": "origin/main",
                            "target_branch": "agent-hub/feature-x",
                            "head_sha": "commit-b"
                        },
                        "timing": {
                            "finished_at": "2026-04-23T10:10:00Z"
                        }
                    }
                }
            }),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: Some("agent-hub/feature-x".to_string()),
        };

        let selected = checkout_ref_for_repo_job(&job, &repo);
        assert_eq!(selected, "commit-b");
    }

    #[test]
    fn resolve_target_branch_for_repo_job_reuses_prior_successful_target_branch_when_unconfigured()
    {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "open_draft_pr".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({
                "job_results": {
                    "open_draft_pr": {
                        "outcome": "succeeded",
                        "repo": {
                            "base_ref": "origin/main",
                            "target_branch": "agent-hub/already-pushed"
                        }
                    }
                }
            }),
            repo: None,
            timeout_secs: None,
            max_retries: 1,
            attempt: 1,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let selected = resolve_target_branch_for_repo_job(&job, &repo);
        assert_eq!(selected, "agent-hub/already-pushed");
    }

    #[test]
    fn resolve_target_branch_for_repo_job_keeps_configured_branch() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "open_draft_pr".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({
                "job_results": {
                    "open_draft_pr": {
                        "outcome": "succeeded",
                        "repo": {
                            "base_ref": "origin/main",
                            "target_branch": "agent-hub/already-pushed"
                        }
                    }
                }
            }),
            repo: None,
            timeout_secs: None,
            max_retries: 1,
            attempt: 1,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: Some("agent-hub/configured".to_string()),
        };

        let selected = resolve_target_branch_for_repo_job(&job, &repo);
        assert_eq!(selected, "agent-hub/configured");
    }

    #[test]
    fn local_agent_egress_plan_maps_llm_only_to_llm_tier() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Llm,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let mut config = LocalAgentConfig::default();
        config.set_config_tree_for_tests(crate::egress_config::ConfigTreeSnapshot {
            network_policies: HashMap::from([(
                "llm-only".to_string(),
                crate::egress_config::ConfiguredNetworkPolicy {
                    hosts: vec!["api.openai.com".to_string()],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        let plan = resolve_local_agent_egress_plan(&runtime, &config).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Llm));
        assert!(!plan.network_none);
    }

    #[test]
    fn local_agent_egress_plan_maps_build_resources_to_build_tier() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Github,
                    NetworkResourceKind::CratesRegistry,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let mut config = LocalAgentConfig::default();
        config.set_config_tree_for_tests(crate::egress_config::ConfigTreeSnapshot {
            network_policies: HashMap::from([(
                "build".to_string(),
                crate::egress_config::ConfiguredNetworkPolicy {
                    hosts: vec!["github.com".to_string(), "api.openai.com".to_string()],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        let plan = resolve_local_agent_egress_plan(&runtime, &config).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Build));
        assert!(!plan.network_none);
    }

    #[test]
    fn local_agent_egress_plan_rejects_unsupported_resources() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Llm,
                    NetworkResourceKind::Github,
                    NetworkResourceKind::GoRegistry,
                    NetworkResourceKind::CratesRegistry,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let mut config = LocalAgentConfig::default();
        config.set_config_tree_for_tests(crate::egress_config::ConfigTreeSnapshot {
            network_policies: HashMap::from([(
                "build".to_string(),
                crate::egress_config::ConfiguredNetworkPolicy {
                    hosts: vec!["github.com".to_string(), "api.openai.com".to_string()],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        let plan = resolve_local_agent_egress_plan(&runtime, &config).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Build));
        assert!(!plan.network_none);

        let full_runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: Some(NetworkResourceRequest::Full),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let mut full_config = LocalAgentConfig::default();
        full_config.set_config_tree_for_tests(crate::egress_config::ConfigTreeSnapshot {
            network_policies: HashMap::from([(
                "full".to_string(),
                crate::egress_config::ConfiguredNetworkPolicy {
                    unrestricted: true,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        let full_plan =
            resolve_local_agent_egress_plan(&full_runtime, &full_config).expect("full egress plan");
        assert_eq!(full_plan.tier, Some(LocalAgentEgressTier::Full));
        assert!(!full_plan.network_none);

        let invalid_runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };
        assert!(resolve_local_agent_egress_plan(&invalid_runtime, &full_config).is_err());
    }

    #[test]
    fn mirror_refresh_stamp_gates_fetch_frequency() {
        let root = std::env::temp_dir().join(format!("mirror-stamp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir");

        assert!(should_refresh_mirror(&root).expect("initial refresh check"));
        write_mirror_refresh_timestamp(&root).expect("write stamp");
        assert!(!should_refresh_mirror(&root).expect("gated refresh check"));

        let stale_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(MIRROR_REFRESH_MIN_SECS + 5);
        std::fs::write(mirror_refresh_stamp_path(&root), stale_secs.to_string())
            .expect("write stale stamp");
        assert!(should_refresh_mirror(&root).expect("stale refresh check"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_workspace_deletes_attempt_branch_best_effort() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root =
                std::env::temp_dir().join(format!("cleanup-branch-{}", uuid::Uuid::new_v4()));
            let mirror = root.join("mirror");
            let source = root.join("source");
            let worktrees_root = root.join("worktrees");
            std::fs::create_dir_all(&source).expect("mkdir source");
            std::fs::create_dir_all(&worktrees_root).expect("mkdir worktrees");

            let init = std::process::Command::new("git")
                .arg("init")
                .arg(&source)
                .status()
                .expect("git init source");
            assert!(init.success());

            let cfg_name = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("config")
                .arg("user.name")
                .arg("Agent Hub")
                .status()
                .expect("git config name");
            assert!(cfg_name.success());
            let cfg_email = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("config")
                .arg("user.email")
                .arg("agent-hub@example.com")
                .status()
                .expect("git config email");
            assert!(cfg_email.success());

            std::fs::write(source.join("README.md"), "hello\n").expect("write readme");
            let add = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("add")
                .arg("README.md")
                .status()
                .expect("git add");
            assert!(add.success());
            let commit = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("commit")
                .arg("-m")
                .arg("init")
                .status()
                .expect("git commit");
            assert!(commit.success());

            let clone = std::process::Command::new("git")
                .arg("clone")
                .arg(&source)
                .arg(&mirror)
                .status()
                .expect("git clone mirror");
            assert!(clone.success());

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                command: "echo".into(),
                args: vec!["ok".into()],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: Some(protocol::RepoSource {
                    repo_url: source.to_string_lossy().to_string(),
                    base_ref: "HEAD".into(),
                    branch_name: Some("agent-hub/manual".into()),
                }),
                timeout_secs: None,
                max_retries: 0,
                attempt: 1,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let workspace_dir = worktree_dir_for_job_attempt(&worktrees_root, &job);
            let attempt_branch = branch_for_job_attempt(&job, Some("agent-hub/manual".into()));
            let add_wt = std::process::Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .arg("worktree")
                .arg("add")
                .arg("-B")
                .arg(&attempt_branch)
                .arg(&workspace_dir)
                .arg("HEAD")
                .status()
                .expect("git worktree add");
            assert!(add_wt.success());

            cleanup_workspace(
                &job,
                &PreparedWorkspace {
                    workspace_dir: workspace_dir.clone(),
                    mirror_dir: Some(mirror.clone()),
                    attempt_branch: Some(attempt_branch.clone()),
                    additional_repo_mounts: Vec::new(),
                },
                None,
            )
            .await
            .expect("cleanup");

            let branch_list = std::process::Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .arg("branch")
                .arg("--list")
                .arg(&attempt_branch)
                .output()
                .expect("git branch list");
            assert!(branch_list.status.success());
            let listed = String::from_utf8(branch_list.stdout).expect("utf8 branch output");
            assert!(listed.trim().is_empty());

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn runtime_spec_from_context_drives_execution_mode_and_timeout() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(77),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };
        let mut ctx = serde_json::json!({});
        protocol::project_runtime_spec_into_job_context(&mut ctx, &runtime);

        let parsed = protocol::canonical_runtime_spec_for_job_effective(&Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "s".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: ctx,
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        assert_eq!(parsed.environment.execution_mode, ExecutionMode::Docker);
        assert_eq!(parsed.environment.timeout_secs, Some(77));
        assert!(!parsed.permissions.can_access_network);
    }

    #[test]
    fn write_agent_context_file_includes_standard_worker_local_paths() {
        let root =
            std::env::temp_dir().join(format!("agent-context-paths-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            AgentContextWriteParams {
                transcript_dir: transcript_dir.to_string_lossy().to_string(),
                workspace_dir: workspace_dir.clone(),
                runtime_spec: runtime.clone(),
                additional_repo_mounts: vec![],
            },
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");
        let paths = parsed
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .expect("paths object");
        assert!(parsed["job"].get("attempt").is_none());
        assert!(parsed["job"].get("max_retries").is_none());
        let mut changed_attempt = job.clone();
        changed_attempt.attempt = 99;
        changed_attempt.max_retries = 99;
        let changed_context_path = transcript_dir.join(".agent-context-changed.json");
        write_agent_context_file(
            &changed_context_path,
            &changed_attempt,
            AgentContextWriteParams {
                transcript_dir: transcript_dir.to_string_lossy().to_string(),
                workspace_dir: workspace_dir.clone(),
                runtime_spec: runtime,
                additional_repo_mounts: vec![],
            },
        )
        .expect("write changed-attempt context");
        let changed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&changed_context_path).expect("read changed context"),
        )
        .expect("parse changed context");
        assert_eq!(parsed["job"], changed["job"]);

        assert_eq!(
            paths
                .get("job_events_jsonl")
                .and_then(serde_json::Value::as_str),
            Some("/transcript/job-events.jsonl")
        );
        assert_eq!(
            paths.get("stdout_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stdout.log")
        );
        assert_eq!(
            paths.get("stderr_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stderr.log")
        );
        assert_eq!(
            paths.get("run_json").and_then(serde_json::Value::as_str),
            Some("/transcript/run.json")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_agent_context_file_uses_container_paths_for_docker_jobs() {
        let root = std::env::temp_dir().join(format!(
            "agent-context-docker-paths-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            AgentContextWriteParams {
                transcript_dir: transcript_dir.to_string_lossy().to_string(),
                workspace_dir: workspace_dir.clone(),
                runtime_spec: runtime,
                additional_repo_mounts: vec![],
            },
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");
        let paths = parsed
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .expect("paths object");

        assert_eq!(
            paths
                .get("workspace_dir")
                .and_then(serde_json::Value::as_str),
            Some("/workspace")
        );
        assert_eq!(
            paths
                .get("transcript_dir")
                .and_then(serde_json::Value::as_str),
            Some("/transcript")
        );
        assert_eq!(
            paths
                .get("job_events_jsonl")
                .and_then(serde_json::Value::as_str),
            Some("/transcript/job-events.jsonl")
        );
        assert_eq!(
            paths.get("stdout_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stdout.log")
        );
        assert_eq!(
            paths.get("stderr_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stderr.log")
        );
        assert_eq!(
            paths.get("run_json").and_then(serde_json::Value::as_str),
            Some("/transcript/run.json")
        );

        let workflow_inputs = parsed
            .get("workflow")
            .and_then(|v| v.get("inputs"))
            .and_then(serde_json::Value::as_object)
            .expect("workflow.inputs object");
        assert_eq!(
            workflow_inputs
                .get("valid_next_events")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert!(
            workflow_inputs
                .get("job_artifact_paths")
                .and_then(serde_json::Value::as_object)
                .is_some()
        );
        assert!(
            workflow_inputs
                .get("job_artifact_metadata")
                .and_then(serde_json::Value::as_object)
                .is_some()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_agent_context_file_includes_workflow_inputs_from_job_context_projection() {
        let root = std::env::temp_dir().join(format!(
            "agent-context-workflow-inputs-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({
                "work_item": {
                    "id": "w-123",
                    "title": "Ship context projection",
                    "description": "Carry work-item summary into local agent context",
                    "labels": ["reactive", "phase-1b"],
                    "priority": "P1",
                    "workflow_state": "build",
                    "workflow_id": "wf"
                },
                "workflow": {
                    "inputs": {
                        "valid_next_events": ["submit", "cancel"],
                        "job_artifact_paths": {
                            "build": {
                                "agent_context": "/tmp/x/.agent-context.json"
                            }
                        },
                        "job_artifact_metadata": {
                            "build": {
                                "agent_context": {
                                    "path": "/tmp/x/.agent-context.json",
                                    "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME
                                }
                            }
                        },
                        "job_artifact_data": {
                            "build": {
                                "agent_context": {"schema": "agent_context_v1"}
                            }
                        }
                    }
                }
            }),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            AgentContextWriteParams {
                transcript_dir: transcript_dir.to_string_lossy().to_string(),
                workspace_dir: workspace_dir.clone(),
                runtime_spec: runtime,
                additional_repo_mounts: vec![],
            },
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");

        assert_eq!(
            parsed["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["submit", "cancel"])
        );
        assert_eq!(
            parsed["workflow"]["inputs"]["job_artifact_paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(
            parsed["workflow"]["inputs"]["job_artifact_metadata"]["build"]["agent_context"]["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            parsed["workflow"]["valid_next_events"],
            serde_json::json!(["submit", "cancel"])
        );
        assert_eq!(parsed["work_item"]["id"], serde_json::json!("w-123"));
        assert_eq!(
            parsed["work_item"]["title"],
            serde_json::json!("Ship context projection")
        );
        assert_eq!(
            parsed["work_item"]["workflow_state"],
            serde_json::json!("build")
        );
        assert_eq!(
            parsed["artifacts"]["paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(parsed["repos"]["primary"], serde_json::Value::Null);
        assert_eq!(parsed["repos"]["additional"], serde_json::json!([]));
        assert!(
            parsed["workflow"]["inputs"]
                .get("job_artifact_data")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_run_json_payload_uses_effective_runtime_and_normalized_docker_paths() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("authorized:image".into()),
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(10),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("runtime:image".into()),
                timeout_secs: Some(77),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
            github_capabilities: Vec::new(),
        };

        let payload = build_run_json_payload(
            &job,
            &runtime,
            &ExecutionOutcome::Succeeded,
            &TerminationReason::ExitCode,
            Some(0),
            "/workspace",
            "/transcript",
        );

        assert_eq!(
            payload.get("schema").and_then(serde_json::Value::as_str),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(
            payload.get("mode").and_then(serde_json::Value::as_str),
            Some("docker")
        );
        assert_eq!(
            payload
                .get("container_image")
                .and_then(serde_json::Value::as_str),
            Some("runtime:image")
        );
        assert_eq!(
            payload
                .get("timeout_secs")
                .and_then(serde_json::Value::as_u64),
            Some(77)
        );
        assert_eq!(
            payload.get("workspace").and_then(serde_json::Value::as_str),
            Some("/workspace")
        );
        assert_eq!(
            payload
                .get("transcript_dir")
                .and_then(serde_json::Value::as_str),
            Some("/transcript")
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let modes = vec![ExecutionMode::Docker];

    let client = match cli.transport {
        TransportKind::Http => Client::Http(HttpClient::new(
            cli.server.trim_end_matches('/').to_string(),
            cli.token.map(Secret::new),
        )),
        TransportKind::Grpc => {
            let channel = tonic::transport::Endpoint::new(cli.grpc_endpoint.clone())?
                .connect()
                .await
                .context("failed to connect gRPC endpoint")?;
            Client::Grpc(GrpcClient {
                client: pb::worker_lifecycle_client::WorkerLifecycleClient::new(channel),
                token: cli.token.map(Secret::new),
            })
        }
    };

    let mut event_publisher = match &client {
        Client::Grpc(grpc) => {
            let tx = start_grpc_event_stream(grpc.client.clone(), grpc.token.clone()).await?;
            Some(JobEventPublisher {
                worker_id: cli.worker_id.clone(),
                tx,
            })
        }
        Client::Http(_) => None,
    };

    let client = Arc::new(client);

    std::fs::create_dir_all(&cli.data_dir)?;
    let mut local_agent_config = load_local_agent_config(cli.local_agent_config.as_deref())
        .context("failed to load local-agent config")?;

    {
        let (resolved_spec, resolved_label) =
            local_agent_config.resolve_default_agent(cli.default_agent.as_deref())?;
        let resolved_clone = resolved_spec.clone();
        eprintln!(
            "info: effective default agent: {} (kind={:?}, command={:?})",
            resolved_label, resolved_clone.kind, resolved_clone.command
        );
        local_agent_config.default_agent = resolved_clone;
    }
    let repo_cache_dir = cli.data_dir.join("repo-cache");
    let remote_repos = validate_triage_repos_and_prime_cache(&local_agent_config, &repo_cache_dir)
        .context("failed to validate configured triage repos")?;
    let _repo_cache_fetcher_handle = spawn_repo_cache_fetcher(
        remote_repos,
        repo_cache_dir.clone(),
        Duration::from_secs(REPO_CACHE_FETCH_INTERVAL_SECS),
    );
    let egress_manager = EgressManager::new(&cli.worker_id, &cli.data_dir, local_agent_config)
        .context("failed to initialize local-agent egress manager")?;
    egress_manager
        .preflight_llm_only()
        .await
        .context("assignment-driven triage requires enforceable agent/model API egress")?;
    let repos_root = cli.data_dir.join("repos");
    std::fs::create_dir_all(&repos_root)?;
    let worktrees_root = cli.data_dir.join("worktrees");
    std::fs::create_dir_all(&worktrees_root)?;
    let artifact_store_root = cli
        .artifact_store_root
        .clone()
        .unwrap_or_else(|| cli.data_dir.clone());
    std::fs::create_dir_all(&artifact_store_root)?;
    let transcripts_root = artifact_store_root.join("transcripts");
    std::fs::create_dir_all(&transcripts_root)?;

    if let Err(error) =
        cleanup_old_transcripts(&transcripts_root, cli.transcript_retention_hours).await
    {
        eprintln!("failed to cleanup old transcripts at startup: {error:#}");
    }
    let mut last_transcript_cleanup = Instant::now();

    client
        .register_worker(&RegisterWorkerRequest {
            id: cli.worker_id.clone(),
            hostname: cli.hostname.clone(),
            supported_modes: modes.clone(),
        })
        .await
        .context("worker registration failed")?;

    loop {
        if last_transcript_cleanup.elapsed()
            >= Duration::from_secs(TRANSCRIPT_CLEANUP_INTERVAL_SECS)
        {
            match cleanup_old_transcripts(&transcripts_root, cli.transcript_retention_hours).await {
                Ok(removed) => {
                    if removed > 0 {
                        eprintln!("info: removed {removed} expired transcript director(ies)");
                    }
                }
                Err(error) => {
                    eprintln!("failed to cleanup old transcripts: {error:#}");
                }
            }
            last_transcript_cleanup = Instant::now();
        }

        if let Err(error) = client.heartbeat(&cli.worker_id, WorkerState::Idle).await {
            eprintln!("worker heartbeat failed: {error}");
            if is_worker_not_found_error(&error) {
                eprintln!("worker not found on server, re-registering...");
                if let Err(register_error) = client
                    .register_worker(&RegisterWorkerRequest {
                        id: cli.worker_id.clone(),
                        hostname: cli.hostname.clone(),
                        supported_modes: modes.clone(),
                    })
                    .await
                {
                    eprintln!("worker re-registration failed: {register_error}");
                } else if let Client::Grpc(grpc) = client.as_ref() {
                    match start_grpc_event_stream(grpc.client.clone(), grpc.token.clone()).await {
                        Ok(tx) => {
                            event_publisher = Some(JobEventPublisher {
                                worker_id: cli.worker_id.clone(),
                                tx,
                            });
                        }
                        Err(stream_error) => {
                            eprintln!("failed to restart gRPC event stream: {stream_error:#}");
                        }
                    }
                }
            }
        }
        let _ = client.dispatch().await;

        let polled_job = match client.poll(&cli.worker_id).await {
            Ok(job) => job,
            Err(error) => {
                eprintln!("poll failed: {error}");
                if is_worker_not_found_error(&error) {
                    eprintln!("worker not found on server, re-registering...");
                    if let Err(register_error) = client
                        .register_worker(&RegisterWorkerRequest {
                            id: cli.worker_id.clone(),
                            hostname: cli.hostname.clone(),
                            supported_modes: modes.clone(),
                        })
                        .await
                    {
                        eprintln!("worker re-registration failed: {register_error}");
                    } else if let Client::Grpc(grpc) = client.as_ref() {
                        match start_grpc_event_stream(grpc.client.clone(), grpc.token.clone()).await
                        {
                            Ok(tx) => {
                                event_publisher = Some(JobEventPublisher {
                                    worker_id: cli.worker_id.clone(),
                                    tx,
                                });
                            }
                            Err(stream_error) => {
                                eprintln!("failed to restart gRPC event stream: {stream_error:#}");
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(cli.poll_interval_secs)).await;
                continue;
            }
        };

        if let Some(job) = polled_job {
            match client
                .update_job(&cli.worker_id, job.id, JobState::Running, None, None)
                .await
            {
                Ok(JobUpdateOutcome::Applied) => {}
                Ok(JobUpdateOutcome::CancelledOnServer) => {
                    eprintln!(
                        "job {} was cancelled before execution started; skipping",
                        job.id
                    );
                    continue;
                }
                Err(error) => {
                    eprintln!("failed to report running job {}: {error}", job.id);
                }
            }

            let hb_client = Arc::clone(&client);
            let hb_worker_id = cli.worker_id.clone();
            let hb_job_id = job.id;
            let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::watch::channel(false);
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let heartbeat_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(JOB_HEARTBEAT_SECS));
                loop {
                    tokio::select! {
                        _ = hb_stop_rx.changed() => break,
                        _ = interval.tick() => {
                            if let Err(error) = hb_client.heartbeat(&hb_worker_id, WorkerState::Busy).await {
                                eprintln!("busy heartbeat failed for job {hb_job_id}: {error}");
                            }
                            match hb_client
                                .update_job(&hb_worker_id, hb_job_id, JobState::Running, None, None)
                                .await
                            {
                                Ok(JobUpdateOutcome::Applied) => {}
                                Ok(JobUpdateOutcome::CancelledOnServer) => {
                                    eprintln!("job {hb_job_id} was cancelled on server; stopping local execution");
                                    let _ = cancel_tx.send(true);
                                    break;
                                }
                                Err(error) => {
                                    eprintln!("running lease renewal failed for job {hb_job_id}: {error}");
                                }
                            }
                        }
                    }
                }
            });

            let transcript_rel_dir = Path::new("transcripts")
                .join(job.id.to_string())
                .to_string_lossy()
                .to_string();
            let transcript_dir = cli
                .artifact_store_root
                .clone()
                .unwrap_or_else(|| cli.data_dir.clone())
                .join(&transcript_rel_dir)
                .to_string_lossy()
                .to_string();

            let result = run_job(
                &job,
                &cli.worker_id,
                &transcript_dir,
                &transcript_rel_dir,
                &cli.data_dir,
                &repos_root,
                &worktrees_root,
                &egress_manager,
                event_publisher.as_ref(),
                cancel_rx,
            )
            .await;

            let _ = hb_stop_tx.send(true);
            let _ = heartbeat_task.await;

            match result {
                Ok(job_result) if matches!(job_result.outcome, ExecutionOutcome::Succeeded) => {
                    if let Err(error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Succeeded,
                            Some(transcript_rel_dir.clone()),
                            Some(job_result),
                        )
                        .await
                    {
                        eprintln!("failed to report successful job {}: {error}", job.id);
                    }
                }
                Ok(job_result) if matches!(job_result.outcome, ExecutionOutcome::Cancelled) => {
                    if let Err(error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Cancelled,
                            Some(transcript_rel_dir.clone()),
                            Some(job_result),
                        )
                        .await
                    {
                        eprintln!("failed to report cancelled job {}: {error}", job.id);
                    }
                }
                Ok(job_result) => {
                    if let Err(error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Failed,
                            Some(transcript_rel_dir.clone()),
                            Some(job_result),
                        )
                        .await
                    {
                        eprintln!("failed to report failed job {}: {error}", job.id);
                    }
                }
                Err(error) => {
                    eprintln!("job {} execution failed: {error}", job.id);
                    let stdout_path = PathBuf::from(&transcript_dir).join("stdout.log");
                    let stderr_path = PathBuf::from(&transcript_dir).join("stderr.log");
                    let now = Utc::now();
                    let infra_result = JobResult {
                        outcome: ExecutionOutcome::Failed,
                        termination_reason: TerminationReason::WorkerError,
                        exit_code: None,
                        timing: ExecutionTiming {
                            started_at: now,
                            finished_at: now,
                            duration_ms: 0,
                        },
                        artifacts: ArtifactSummary {
                            stdout_path: artifact_store_relative_path(
                                &transcript_rel_dir,
                                "stdout.log",
                            ),
                            stderr_path: artifact_store_relative_path(
                                &transcript_rel_dir,
                                "stderr.log",
                            ),
                            output_path: None,
                            transcript_dir: Some(transcript_rel_dir.clone()),
                            stdout_bytes: file_len_u64(&stdout_path),
                            stderr_bytes: file_len_u64(&stderr_path),
                            output_bytes: None,
                            structured_transcript_artifacts: Vec::new(),
                        },
                        output_json: None,
                        repo: None,
                        github_pr: None,
                        error_message: Some(format!("{error:#}")),
                    };
                    if let Err(update_error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Failed,
                            Some(transcript_rel_dir.clone()),
                            Some(infra_result),
                        )
                        .await
                    {
                        eprintln!("failed to report failed job {}: {update_error}", job.id);
                    }
                }
            }

            if let Err(error) = client.heartbeat(&cli.worker_id, WorkerState::Idle).await {
                eprintln!("worker heartbeat failed after job {}: {error}", job.id);
            }
        }

        tokio::time::sleep(Duration::from_secs(cli.poll_interval_secs)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_job(
    job: &Job,
    worker_id: &str,
    transcript_dir: &str,
    transcript_rel_dir: &str,
    _data_dir: &Path,
    repos_root: &Path,
    worktrees_root: &Path,
    egress_manager: &EgressManager,
    event_publisher: Option<&JobEventPublisher>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<JobResult> {
    let local_agent_config = egress_manager.config();
    let verified = verify_authorized_job(job, local_agent_config)?;
    let job = &verified.job;
    let runtime_spec = verified.runtime_spec;
    let resolved_runner = verified.runner;
    let work_item_context = verified.work_item_context;
    let triage_job = verified.triage;
    let github_action = verified.github_action;

    let transcript_dir = resolve_to_absolute(Path::new(transcript_dir))
        .with_context(|| format!("resolve transcript dir {}", transcript_dir))?
        .to_string_lossy()
        .to_string();
    let transcript_dir = transcript_dir.as_str();

    std::fs::create_dir_all(transcript_dir)?;

    let environment_table =
        EnvironmentValidationTable::from_local_agent_config(local_agent_config)?;
    validate_triage_mount_and_credential_requests(
        triage_job,
        &resolved_runner,
        &runtime_spec,
        local_agent_config,
    )?;
    let local_egress_plan = resolve_local_agent_egress_plan(&runtime_spec, local_agent_config)?;
    let runtime_source_label = "immutable_authorization";
    eprintln!(
        "info: job {} using immutable authorization {} (version={}, mode={}, timeout_secs={:?}, network={:?}, mounts={}, toolchains={})",
        job.id,
        job.context
            .get(protocol::CONTEXT_REQUEST_DIGEST_KEY)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("missing"),
        runtime_spec.runtime_version,
        runtime_spec.environment.execution_mode,
        runtime_spec.environment.timeout_secs,
        runtime_spec.environment.network_resources,
        runtime_spec.environment.file_mounts.len(),
        runtime_spec.environment.toolchains.len()
    );

    let job_events_path = PathBuf::from(transcript_dir).join("job-events.jsonl");
    let job_event_sink = JobEventSink {
        worker_id: event_publisher
            .map(|p| p.worker_id.clone())
            .unwrap_or_else(|| worker_id.to_string()),
        writer: Arc::new(Mutex::new(
            JobEventTranscriptWriter::create(&job_events_path).await?,
        )),
        publisher: event_publisher.cloned(),
    };

    job_event_sink
        .emit(
            job.id,
            pb::JobEventKind::JobEventSystem,
            "workspace preparation started".to_string(),
        )
        .await;

    let prepared_workspace =
        prepare_workspace(job, &runtime_spec, repos_root, worktrees_root).await?;
    let workspace_dir = prepared_workspace.workspace_dir.clone();

    job_event_sink
        .emit(
            job.id,
            pb::JobEventKind::JobEventSystem,
            format!("workspace prepared at {}", workspace_dir.to_string_lossy()),
        )
        .await;

    let mut env = HashMap::new();
    for (k, v) in &job.env {
        env.insert(k.clone(), v.clone());
    }

    let prompt_dir = PathBuf::from(transcript_dir).join("prompt");
    std::fs::create_dir_all(&prompt_dir)?;
    if let Some(prompt) = &job.prompt {
        if let Some(system) = &prompt.system {
            std::fs::write(prompt_dir.join("system.txt"), system)?;
        }
        std::fs::write(prompt_dir.join("user.txt"), &prompt.user)?;
    }
    std::fs::write(
        PathBuf::from(transcript_dir).join("context.json"),
        serde_json::to_vec_pretty(&job.context)?,
    )?;

    let prompt_system_path = prompt_dir.join("system.txt");
    let prompt_user_path = prompt_dir.join("user.txt");
    let context_path = PathBuf::from(transcript_dir).join("context.json");
    let agent_context_path = PathBuf::from(transcript_dir).join(".agent-context.json");
    let worktree_agent_context_path = workspace_dir.join(".agent-context.json");
    let transcript_jsonl_path = PathBuf::from(transcript_dir).join("transcript.jsonl");
    let output_path = PathBuf::from(transcript_dir).join("output.json");
    let stdout_path = PathBuf::from(transcript_dir).join("stdout.log");
    let stderr_path = PathBuf::from(transcript_dir).join("stderr.log");
    let run_json_path = PathBuf::from(transcript_dir).join("run.json");
    if let Some(action) = github_action.as_ref() {
        let started_at = Utc::now();
        let operation = async {
            let output_json = execute_github_action(
                action,
                &runtime_spec,
                &workspace_dir,
                local_agent_config,
                &work_item_context,
            )
            .await?;
            std::fs::write(&output_path, serde_json::to_vec_pretty(&output_json)?)?;
            std::fs::write(&stdout_path, b"")?;
            std::fs::write(&stderr_path, b"")?;
            std::fs::write(&transcript_jsonl_path, b"")?;
            let finished_at = Utc::now();
            let timing = ExecutionTiming {
                started_at,
                finished_at,
                duration_ms: (finished_at - started_at)
                    .to_std()
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or(0),
            };
            let target_branch = job
                .repo
                .as_ref()
                .map(|repo| resolve_target_branch_for_repo_job(job, repo));
            let repo =
                capture_repo_metadata(&workspace_dir, job.repo.as_ref(), target_branch).await;
            std::fs::write(
                &run_json_path,
                serde_json::to_vec_pretty(&build_run_json_payload(
                    job,
                    &runtime_spec,
                    &ExecutionOutcome::Succeeded,
                    &TerminationReason::ExitCode,
                    Some(0),
                    &workspace_dir.to_string_lossy(),
                    transcript_dir,
                ))?,
            )?;
            Ok::<JobResult, anyhow::Error>(JobResult {
                outcome: ExecutionOutcome::Succeeded,
                termination_reason: TerminationReason::ExitCode,
                exit_code: Some(0),
                timing,
                artifacts: ArtifactSummary {
                    stdout_path: stdout_path.to_string_lossy().to_string(),
                    stderr_path: stderr_path.to_string_lossy().to_string(),
                    output_path: Some(output_path.to_string_lossy().to_string()),
                    transcript_dir: Some(transcript_dir.to_string()),
                    stdout_bytes: Some(0),
                    stderr_bytes: Some(0),
                    output_bytes: file_len_u64(&output_path),
                    structured_transcript_artifacts: collect_structured_transcript_artifacts(
                        &agent_context_path,
                        &run_json_path,
                        &transcript_jsonl_path,
                        &job_events_path,
                    ),
                },
                github_pr: output_json
                    .as_object()
                    .and_then(|_| parse_github_pr_from_output(&output_json)),
                output_json: Some(output_json),
                repo,
                error_message: None,
            })
        };
        let operation = run_authorized_github_operation(
            operation,
            runtime_spec.environment.timeout_secs,
            cancel_rx.clone(),
        )
        .await;
        return finalize_required_cleanup(
            operation,
            cleanup_workspace(job, &prepared_workspace, Some(&job_event_sink)),
        )
        .await;
    }
    env.insert(
        "AGENT_WORKSPACE_DIR".to_string(),
        workspace_dir.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        transcript_dir.to_string(),
    );
    if let Some(repo) = &job.repo {
        let allowlist = local_agent_config.github_host_allowlist();
        if let Some(repo_slug) =
            normalize_github_repo_slug_with_allowlist(&repo.repo_url, &allowlist)
        {
            env.insert("AGENT_REPO_SLUG".to_string(), repo_slug);
        }
        if let Some(parsed) = parse_github_repo_url(&repo.repo_url) {
            // Always export the parsed host so downstream tooling (and human
            // debugging) can see what we resolved, even when the host isn't
            // allowlisted. The slug above is the one that gates `gh` usage.
            env.insert("AGENT_REPO_HOST".to_string(), parsed.host);
        }
        env.insert("AGENT_REPO_URL".to_string(), repo.repo_url.clone());
        env.insert("AGENT_BASE_REF".to_string(), repo.base_ref.clone());
        let target_branch = resolve_target_branch_for_repo_job(job, repo);
        env.insert("AGENT_TARGET_BRANCH".to_string(), target_branch);
    }
    env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        context_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        agent_context_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        transcript_jsonl_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        job_events_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        output_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        stdout_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        stderr_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        run_json_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        prompt_user_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        prompt_system_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_CAN_ACCESS_NETWORK".to_string(),
        runtime_spec.permissions.can_access_network.to_string(),
    );
    if let Some(tier) = local_egress_plan.tier {
        env.insert(
            "AGENT_LOCAL_EGRESS_TIER".to_string(),
            tier.as_str().to_string(),
        );
    }
    env.insert(
        "AGENT_PERMISSION_TIER".to_string(),
        planned_permission_tier_name(&runtime_spec.permissions.tier).to_string(),
    );
    env.insert(
        "AGENT_REPO_COUNT".to_string(),
        runtime_spec.repos.len().to_string(),
    );
    if let Some(approval_rules) = &runtime_spec.approval_rules
        && let Ok(value) = serde_json::to_string(approval_rules)
    {
        env.insert("AGENT_APPROVAL_RULES".to_string(), value);
    }
    env.insert(
        "AGENT_APPROVAL_MODE".to_string(),
        approval_mode_from_job_context(&job.context).to_string(),
    );
    if let Some(work_item_title) = work_item_context
        .get("title")
        .and_then(serde_json::Value::as_str)
    {
        env.insert(
            "AGENT_HUB_WORK_ITEM_TITLE".to_string(),
            work_item_title.to_string(),
        );
    }
    if let Some(work_item_workflow_state) = work_item_context
        .get("workflow_state")
        .and_then(serde_json::Value::as_str)
    {
        env.insert(
            "AGENT_HUB_WORK_ITEM_WORKFLOW_STATE".to_string(),
            work_item_workflow_state.to_string(),
        );
    }
    if let Some(owner_login) = work_item_context
        .get("owner_github_login")
        .and_then(serde_json::Value::as_str)
    {
        env.insert("AGENT_OWNER_LOGIN".to_string(), owner_login.to_string());
    }
    if let Some(task_title) = job
        .context
        .get("task_title")
        .and_then(serde_json::Value::as_str)
    {
        env.insert("AGENT_TASK_TITLE".to_string(), task_title.to_string());
    }
    if let Some(task_description) = job
        .context
        .get("task_description")
        .and_then(serde_json::Value::as_str)
    {
        env.insert(
            "AGENT_TASK_DESCRIPTION".to_string(),
            task_description.to_string(),
        );
    }
    // NOTE: permission-related env vars remain advisory metadata for in-agent behavior,
    // while runtime boundary enforcement is active for Docker execution.

    if let Err(error) = write_agent_context_file(
        &agent_context_path,
        job,
        AgentContextWriteParams {
            transcript_dir: transcript_dir.to_string(),
            workspace_dir: workspace_dir.clone(),
            runtime_spec: runtime_spec.clone(),
            additional_repo_mounts: prepared_workspace
                .additional_repo_mounts
                .iter()
                .map(|mount| AdditionalRepoMountView {
                    repo_url: mount.repo_url.clone(),
                    container_path: mount.container_path.clone(),
                    writable: mount.writable,
                })
                .collect(),
        },
    ) {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("warning: failed to write .agent-context.json: {error:#}"),
            )
            .await;
    } else if let Err(error) = std::fs::copy(&agent_context_path, &worktree_agent_context_path) {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("warning: failed to write worktree .agent-context.json: {error:#}"),
            )
            .await;
    }

    let resolved_job = Job {
        command: resolved_runner.command.clone(),
        args: resolved_runner.args.clone(),
        container_image: runtime_spec.environment.container_image.clone(),
        ..job.clone()
    };

    let mut execution_result = async {
        let started_at = Utc::now();
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "command start: {} {}",
                    resolved_runner.command,
                    resolved_runner.args.join(" ")
                ),
            )
            .await;
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "runtime source: {} (mode={}, timeout_secs={:?}, network={:?})",
                    runtime_source_label,
                    runtime_spec.environment.execution_mode,
                    runtime_spec.environment.timeout_secs,
                    runtime_spec.environment.network_resources
                ),
            )
            .await;
        let execution_status = match &resolved_runner.kind {
            ResolvedRunnerKind::Opencode(opencode) => {
                run_docker_opencode_with_validation_retries(
                    &resolved_job,
                    local_agent_config,
                    opencode,
                    &workspace_dir,
                    transcript_dir,
                    &transcript_jsonl_path,
                    &stdout_path,
                    &stderr_path,
                    &output_path,
                    &env,
                    runtime_spec.environment.timeout_secs,
                    runtime_spec.environment.container_image.clone(),
                    &runtime_spec,
                    &local_egress_plan,
                    egress_manager,
                    prepared_workspace.mirror_dir.as_deref(),
                    &prepared_workspace.additional_repo_mounts,
                    workflow_valid_next_events_from_job_context(&job.context).unwrap_or_default(),
                    work_item_context.clone(),
                    &environment_table,
                    Some(&job_event_sink),
                    Some(cancel_rx.clone()),
                )
                .await?
            }
            ResolvedRunnerKind::Generic => {
                run_docker(
                    &resolved_job,
                    local_agent_config,
                    &workspace_dir,
                    transcript_dir,
                    &transcript_jsonl_path,
                    &stdout_path,
                    &stderr_path,
                    &env,
                    runtime_spec.environment.timeout_secs,
                    runtime_spec.environment.container_image.clone(),
                    &runtime_spec,
                    &local_egress_plan,
                    egress_manager,
                    prepared_workspace.mirror_dir.as_deref(),
                    &prepared_workspace.additional_repo_mounts,
                    Some(&job_event_sink),
                    Some(cancel_rx.clone()),
                )
                .await?
            }
        };
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at)
            .to_std()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let (outcome, termination_reason, exit_code) = match execution_status {
            ExecutionStatus::Exited(code) => {
                if code == 0 {
                    (
                        ExecutionOutcome::Succeeded,
                        TerminationReason::ExitCode,
                        Some(code),
                    )
                } else {
                    (
                        ExecutionOutcome::Failed,
                        TerminationReason::ExitCode,
                        Some(code),
                    )
                }
            }
            ExecutionStatus::TimedOut => {
                job_event_sink
                    .emit(
                        job.id,
                        pb::JobEventKind::JobEventSystem,
                        "execution timed out".to_string(),
                    )
                    .await;
                (ExecutionOutcome::TimedOut, TerminationReason::Timeout, None)
            }
            ExecutionStatus::Cancelled => {
                job_event_sink
                    .emit(
                        job.id,
                        pb::JobEventKind::JobEventSystem,
                        "execution cancelled by server".to_string(),
                    )
                    .await;
                (
                    ExecutionOutcome::Cancelled,
                    TerminationReason::Cancelled,
                    None,
                )
            }
        };

        let output_json = if output_path.exists() {
            job_event_sink
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    format!("output.json detected at {}", output_path.to_string_lossy()),
                )
                .await;
            let bytes = std::fs::read(&output_path)?;
            let raw_output_json: serde_json::Value =
                serde_json::from_slice(&bytes).context("invalid output.json")?;

            if let Ok(typed_output) = serde_json::from_value::<AgentOutput>(raw_output_json.clone())
                && let Some(valid_next_events) =
                    workflow_valid_next_events_from_job_context(&job.context)
                && let Err(validation_error) =
                    validate_agent_output_next_event(&typed_output, &valid_next_events)
            {
                let message = format!(
                    "warning: output.json next_event validation failed for job {}: {}",
                    job.id, validation_error
                );
                eprintln!("{message}");
                job_event_sink
                    .emit(job.id, pb::JobEventKind::JobEventSystem, message)
                    .await;
            }

            Some(raw_output_json)
        } else {
            None
        };

        let github_pr = output_json.as_ref().and_then(parse_github_pr_from_output);

        let target_branch = job
            .repo
            .as_ref()
            .map(|repo| resolve_target_branch_for_repo_job(job, repo));
        let repo_meta =
            capture_repo_metadata(&workspace_dir, job.repo.as_ref(), target_branch).await;

        let (workspace_dir_display, transcript_dir_display) =
            display_paths_for_runtime(&workspace_dir, transcript_dir, &runtime_spec);
        let mut meta = tokio::fs::File::create(&run_json_path).await?;
        let payload = build_run_json_payload(
            job,
            &runtime_spec,
            &outcome,
            &termination_reason,
            exit_code,
            &workspace_dir_display,
            &transcript_dir_display,
        );
        meta.write_all(serde_json::to_string_pretty(&payload)?.as_bytes())
            .await?;

        Ok::<JobResult, anyhow::Error>(JobResult {
            outcome,
            termination_reason,
            exit_code,
            timing: ExecutionTiming {
                started_at,
                finished_at,
                duration_ms,
            },
            artifacts: ArtifactSummary {
                stdout_path: artifact_store_relative_path(transcript_rel_dir, "stdout.log"),
                stderr_path: artifact_store_relative_path(transcript_rel_dir, "stderr.log"),
                output_path: if output_path.exists() {
                    Some(artifact_store_relative_path(
                        transcript_rel_dir,
                        "output.json",
                    ))
                } else {
                    None
                },
                transcript_dir: Some(transcript_rel_dir.to_string()),
                stdout_bytes: file_len_u64(&stdout_path),
                stderr_bytes: file_len_u64(&stderr_path),
                output_bytes: file_len_u64(&output_path),
                structured_transcript_artifacts: Vec::new(),
            },
            output_json,
            repo: repo_meta,
            github_pr,
            error_message: None,
        })
    }
    .await;

    let _ = std::fs::remove_file(&worktree_agent_context_path);

    if let Err(error) = cleanup_workspace(job, &prepared_workspace, Some(&job_event_sink)).await {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("workspace cleanup warning: {error:#}"),
            )
            .await;
    }

    if let Ok(job_result) = &mut execution_result {
        job_result.artifacts.structured_transcript_artifacts =
            collect_structured_transcript_artifacts(
                &agent_context_path,
                &run_json_path,
                &transcript_jsonl_path,
                &job_events_path,
            )
            .into_iter()
            .map(|mut artifact| {
                artifact.path = artifact_store_relative_path(
                    transcript_rel_dir,
                    &artifact_path_tail(&artifact.path),
                );
                artifact
            })
            .collect();
    }

    execution_result
}

struct VerifiedAuthorizedJob {
    job: Job,
    runtime_spec: protocol::CanonicalRuntimeSpec,
    runner: ResolvedRunnerSpec,
    work_item_context: serde_json::Value,
    triage: bool,
    github_action: Option<protocol::GithubAction>,
}

fn verify_authorized_job(
    job: &Job,
    config: &LocalAgentConfig,
) -> anyhow::Result<VerifiedAuthorizedJob> {
    let authorization_id = job
        .context
        .get(protocol::CONTEXT_AUTHORIZATION_ID_KEY)
        .and_then(serde_json::Value::as_str)
        .context("job has no immutable authorization identity")?;
    if authorization_id.trim().is_empty() {
        anyhow::bail!("job authorization identity must not be empty");
    }
    let request_digest = job
        .context
        .get(protocol::CONTEXT_REQUEST_DIGEST_KEY)
        .and_then(serde_json::Value::as_str)
        .context("job has no immutable request digest")?;
    let request = protocol::authorization_request_from_job_context(&job.context)
        .context("job has no resolved authorization request")?;
    if request.work_unit_id != job.work_item_id || request.state_id != job.step_id {
        anyhow::bail!("job authorization work-unit binding mismatch");
    }
    if request.request_digest()? != request_digest {
        anyhow::bail!("job authorization request digest mismatch");
    }
    if request.permission_envelope.environment.execution_mode != ExecutionMode::Docker {
        anyhow::bail!("only authorized Docker jobs are supported");
    }
    if !request.matches_job_execution(job) {
        anyhow::bail!("job execution fields do not match immutable authorization");
    }
    if request.behavior.triage != (request.workflow_id == "__triage__")
        || request.behavior.allow_remote_credentials == request.behavior.triage
    {
        anyhow::bail!("job triage or credential classification is invalid");
    }
    let signed_work_item_context =
        work_item_contract_from_job_context(&request.behavior.execution_context);
    if is_triage_job(&signed_work_item_context) != request.behavior.triage
        || job_allows_remote_credentials(&signed_work_item_context)
            != request.behavior.allow_remote_credentials
    {
        anyhow::bail!("signed work-item classification does not match authorization");
    }
    let mut authorized_primary_repo = request.behavior.primary_repo.clone();
    match (
        authorized_primary_repo.as_mut(),
        request.behavior.primary_repo_branch.as_deref(),
    ) {
        (Some(repo), Some(branch)) if !branch.trim().is_empty() => {
            repo.branch_name = Some(branch.to_string());
        }
        (None, None) => {}
        _ => anyhow::bail!("authorized primary repository branch binding is invalid"),
    }
    if let Some(environment_name) = request
        .resolved_environment
        .get("base")
        .and_then(serde_json::Value::as_str)
    {
        let local_environment = config
            .config_tree()
            .and_then(|tree| {
                tree.environments
                    .iter()
                    .find(|environment| environment.name == environment_name)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("authorized environment '{environment_name}' is not loaded")
            })?;
        let definition = request
            .resolved_environment
            .get("definition")
            .and_then(serde_json::Value::as_object)
            .context("authorized environment has no resolved definition")?;
        let authorized_bundle = definition
            .get("bundle_digests")
            .cloned()
            .context("authorized environment has no bundle digest")?;
        let authorized_policies = definition
            .get("network_policy_bundle_digests")
            .cloned()
            .context("authorized environment has no network-policy digest")?;
        if authorized_bundle != serde_json::to_value(&local_environment.bundle_digests)?
            || authorized_policies
                != serde_json::to_value(&local_environment.network_policy_bundle_digests)?
        {
            anyhow::bail!("local environment bundle does not match authorization snapshot");
        }
    }
    if request.behavior.execution_kind == "command" {
        if request.behavior.runner_profile
            != serde_json::json!({"profile_name":"command", "config_digest":"command_v1"})
        {
            anyhow::bail!("authorized command runner profile is invalid");
        }
        let mut authorized_job = job.clone();
        authorized_job.command = request.behavior.command;
        authorized_job.args = request.behavior.args;
        authorized_job.env = request.behavior.env.into_iter().collect();
        authorized_job.prompt = request.behavior.prompt;
        authorized_job.repo = authorized_primary_repo;
        authorized_job.context =
            authorized_execution_context(job, request.behavior.execution_context);
        return Ok(VerifiedAuthorizedJob {
            runtime_spec: request.permission_envelope,
            runner: ResolvedRunnerSpec {
                kind: ResolvedRunnerKind::Generic,
                command: authorized_job.command.clone(),
                args: authorized_job.args.clone(),
                file_mounts: Vec::new(),
                requires_credentials: Vec::new(),
            },
            work_item_context: work_item_contract_from_job_context(&authorized_job.context),
            triage: request.behavior.triage,
            github_action: None,
            job: authorized_job,
        });
    }
    if request.behavior.execution_kind == "managed" {
        if request.behavior.runner_profile
            != serde_json::json!({"profile_name":"github_action", "config_digest":"github_action_v1"})
        {
            anyhow::bail!("authorized GitHub action profile is invalid");
        }
        let operation = request
            .behavior
            .execution_context
            .get("workflow")
            .and_then(|workflow| workflow.get("managed_operation"))
            .and_then(serde_json::Value::as_str)
            .context("managed GitHub action has no operation")?
            .parse::<protocol::GithubAction>()
            .context("managed GitHub action is unknown")?;
        validate_github_action_capability(
            &operation,
            &request.permission_envelope,
            authorized_primary_repo.as_ref(),
        )?;
        let mut authorized_job = job.clone();
        authorized_job.command.clear();
        authorized_job.args.clear();
        authorized_job.env.clear();
        authorized_job.prompt = None;
        authorized_job.repo = authorized_primary_repo;
        authorized_job.context =
            authorized_execution_context(job, request.behavior.execution_context);
        return Ok(VerifiedAuthorizedJob {
            runtime_spec: request.permission_envelope,
            runner: ResolvedRunnerSpec {
                kind: ResolvedRunnerKind::Generic,
                command: String::new(),
                args: Vec::new(),
                file_mounts: Vec::new(),
                requires_credentials: Vec::new(),
            },
            work_item_context: work_item_contract_from_job_context(&authorized_job.context),
            triage: false,
            github_action: Some(operation),
            job: authorized_job,
        });
    }
    let profile: agent_hub_config::ResolvedRunnerProfile =
        serde_json::from_value(request.behavior.runner_profile.clone())
            .context("authorization has invalid runner profile")?;
    if agent_hub_config::digest_profile(&profile).map_err(anyhow::Error::msg)?
        != profile.config_digest
    {
        anyhow::bail!("authorized runner profile digest mismatch");
    }
    let configured_profiles = RunnerProfilesConfig {
        default_agent: config.default_agent.clone(),
        default_agent_id: config.default_agent_id.clone(),
        managed_runners: config.managed_runners.clone(),
        credential_providers: config
            .credential_providers
            .iter()
            .map(|(name, provider)| Ok((name.clone(), serde_json::to_value(provider)?)))
            .collect::<Result<HashMap<_, _>, serde_json::Error>>()?,
        mcp_bridges: config
            .mcp_bridges
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?,
        egress: serde_json::to_value(&config.egress)?,
        github_hosts: config.github_hosts.clone(),
    };
    let configured = if request.behavior.execution_kind == "managed" {
        configured_profiles
            .resolve_managed_profile(&profile.profile_name)
            .map_err(anyhow::Error::msg)?
    } else {
        configured_profiles
            .resolve_agent_profile(if profile.profile_name == "default_agent" {
                None
            } else {
                Some(profile.profile_name.as_str())
            })
            .map_err(anyhow::Error::msg)?
    };
    if configured != profile {
        anyhow::bail!("local runner profile does not match authorized config digest");
    }
    let runner = ResolvedRunnerSpec {
        kind: match profile.kind {
            ManagedRunnerKind::Opencode => ResolvedRunnerKind::Opencode(OpencodeManagedSpec {
                max_validation_attempts: profile.opencode.max_validation_attempts,
            }),
            ManagedRunnerKind::Generic => ResolvedRunnerKind::Generic,
        },
        command: profile.command,
        args: profile.args,
        file_mounts: Vec::new(),
        requires_credentials: profile
            .credential_providers
            .into_iter()
            .map(|provider| provider.name)
            .collect(),
    };
    let mut authorized_job = job.clone();
    authorized_job.command = request.behavior.command;
    authorized_job.args = request.behavior.args;
    authorized_job.env = request.behavior.env.into_iter().collect();
    authorized_job.prompt = request.behavior.prompt;
    authorized_job.repo = authorized_primary_repo;
    authorized_job.context = authorized_execution_context(job, request.behavior.execution_context);
    Ok(VerifiedAuthorizedJob {
        runtime_spec: request.permission_envelope,
        runner,
        work_item_context: work_item_contract_from_job_context(&authorized_job.context),
        triage: request.behavior.triage,
        github_action: None,
        job: authorized_job,
    })
}

fn validate_github_action_capability(
    action: &protocol::GithubAction,
    runtime: &protocol::CanonicalRuntimeSpec,
    repo: Option<&protocol::RepoSource>,
) -> anyhow::Result<()> {
    let capability = runtime
        .github_capabilities
        .as_slice()
        .first()
        .filter(|_| runtime.github_capabilities.len() == 1)
        .context("managed GitHub action requires exactly one capability")?;
    let repo = repo.context("managed GitHub action requires a repository")?;
    let parsed = parse_github_repo_url(&repo.repo_url)
        .context("managed GitHub action repository is not GitHub-shaped")?;
    if capability.host != parsed.host || capability.repository != parsed.slug {
        anyhow::bail!("managed GitHub action repository does not match authorization");
    }
    if protocol::canonical_github_api_base_for_host(&capability.host, &capability.api_base_url)
        .is_none()
    {
        anyhow::bail!("managed GitHub action API base URL is invalid");
    }
    let branch = repo
        .branch_name
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .context("managed GitHub action requires an exact branch")?;
    if capability.branch.as_deref() != Some(branch)
        || capability.head_branch.as_deref() != Some(branch)
        || capability.base_branch.as_deref() != Some(repo.base_ref.trim_start_matches("origin/"))
    {
        anyhow::bail!("managed GitHub action branch constraints do not match authorization");
    }
    let required_operations = action.required_operations();
    let mut actual_operations = capability.operations.clone();
    actual_operations.sort();
    let mut required_operations = required_operations.to_vec();
    required_operations.sort();
    if actual_operations != required_operations
        || capability.credential_provider != format!("github:{}:read_write", capability.host)
    {
        anyhow::bail!("managed GitHub action exceeds its signed capability");
    }
    match action {
        protocol::GithubAction::PushHead | protocol::GithubAction::ReadOrCreatePullRequest => {}
        _ if capability.pull_request.is_none() => {
            anyhow::bail!("managed GitHub action requires an exact pull request")
        }
        _ => {}
    }
    if matches!(action, protocol::GithubAction::MergePullRequest)
        && capability.expected_head_sha.is_none()
    {
        anyhow::bail!("merge capability requires an exact expected head SHA");
    }
    Ok(())
}

async fn run_authorized_github_operation<F, T>(
    operation: F,
    timeout_secs: Option<u64>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let cancellation = wait_for_cancellation(cancel_rx);
    tokio::pin!(operation);
    tokio::pin!(cancellation);
    if let Some(timeout_secs) = timeout_secs {
        tokio::select! {
            result = &mut operation => result,
            () = &mut cancellation => anyhow::bail!("managed GitHub action cancelled"),
            () = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                anyhow::bail!("managed GitHub action exceeded authorized timeout")
            }
        }
    } else {
        tokio::select! {
            result = &mut operation => result,
            () = &mut cancellation => anyhow::bail!("managed GitHub action cancelled"),
        }
    }
}

async fn wait_for_cancellation(mut cancel_rx: watch::Receiver<bool>) {
    if *cancel_rx.borrow() {
        return;
    }
    while cancel_rx.changed().await.is_ok() {
        if *cancel_rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

async fn finalize_required_cleanup<T, F>(
    operation: anyhow::Result<T>,
    cleanup: F,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let cleanup = cleanup.await;
    match (operation, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => {
            Err(error.context(format!("workspace cleanup failed: {cleanup_error:#}")))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct GithubHttpRequest {
    method: reqwest::Method,
    url: String,
    query: Vec<(String, String)>,
    body: Option<serde_json::Value>,
}

#[derive(Debug)]
struct GithubHttpResponse {
    status: reqwest::StatusCode,
    body: serde_json::Value,
}

trait GithubHttpTransport {
    async fn send(
        &self,
        request: GithubHttpRequest,
        token: &str,
    ) -> anyhow::Result<GithubHttpResponse>;
}

struct ReqwestGithubHttpTransport {
    client: reqwest::Client,
}

impl ReqwestGithubHttpTransport {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("agent-hub-github-action/1")
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }
}

impl GithubHttpTransport for ReqwestGithubHttpTransport {
    async fn send(
        &self,
        request: GithubHttpRequest,
        token: &str,
    ) -> anyhow::Result<GithubHttpResponse> {
        let mut builder = self
            .client
            .request(request.method, &request.url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if !request.query.is_empty() {
            builder = builder.query(&request.query);
        }
        if let Some(body) = request.body {
            builder = builder.json(&body);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("GitHub request failed with HTTP {status}");
        }
        let bytes = response.bytes().await?;
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).context("GitHub returned invalid JSON")?
        };
        Ok(GithubHttpResponse { status, body })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitCommandRequest {
    workspace: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
}

#[derive(Debug)]
struct GitCommandOutput {
    success: bool,
    stdout: Vec<u8>,
}

trait GitCommandRunner {
    async fn run(&self, request: GitCommandRequest) -> anyhow::Result<GitCommandOutput>;
}

struct TokioGitCommandRunner;

impl GitCommandRunner for TokioGitCommandRunner {
    async fn run(&self, request: GitCommandRequest) -> anyhow::Result<GitCommandOutput> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(request.workspace)
            .args(request.args)
            .envs(request.env)
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(GitCommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

struct TemporaryCredentialFile {
    path: Option<PathBuf>,
}

impl TemporaryCredentialFile {
    fn create(contents: &[u8]) -> anyhow::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("agent-hub-github-askpass-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, contents)?;
        Ok(Self { path: Some(path) })
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("temporary credential path")
    }

    fn cleanup(mut self) -> anyhow::Result<()> {
        let path = self.path.take().expect("temporary credential path");
        std::fs::remove_file(&path)
            .with_context(|| format!("remove GitHub credential helper {}", path.display()))
    }
}

impl Drop for TemporaryCredentialFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn execute_github_action(
    action: &protocol::GithubAction,
    runtime: &protocol::CanonicalRuntimeSpec,
    workspace: &Path,
    config: &LocalAgentConfig,
    work_item_context: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let capability = runtime
        .github_capabilities
        .as_slice()
        .first()
        .filter(|_| runtime.github_capabilities.len() == 1)
        .context("managed GitHub action requires exactly one capability")?;
    let token = resolve_github_action_token(config, capability).await?;
    let transport = ReqwestGithubHttpTransport::new()?;
    execute_github_action_with(
        action,
        runtime,
        workspace,
        work_item_context,
        &token,
        &transport,
        &TokioGitCommandRunner,
    )
    .await
}

fn github_read_write_provider(host: &str) -> String {
    format!("github:{host}:read_write")
}

async fn resolve_github_action_token(
    config: &LocalAgentConfig,
    capability: &protocol::GithubCapability,
) -> anyhow::Result<String> {
    if !config
        .github_host_allowlist()
        .iter()
        .any(|host| host.eq_ignore_ascii_case(&capability.host))
    {
        anyhow::bail!("authorized GitHub host is not configured on this worker");
    }
    let expected_provider = github_read_write_provider(&capability.host);
    if capability.credential_provider != expected_provider {
        anyhow::bail!("GitHub credential provider does not match host and access class");
    }
    let configured_api_base = config
        .github_api_base_urls
        .get(&capability.host)
        .context("authorized GitHub host has no configured API base URL")?;
    let authorized_api_base =
        protocol::canonical_github_api_base_for_host(&capability.host, &capability.api_base_url)
            .context("authorized GitHub API base URL is invalid")?;
    if configured_api_base != &authorized_api_base {
        anyhow::bail!("GitHub API base URL does not match authorized host configuration");
    }
    let credentials = resolve_configured_credentials(config, &[expected_provider])
        .await
        .map_err(|_| anyhow::anyhow!("GitHub credential provider resolution failed"))?;
    github_token_from_credentials(credentials)
}

fn github_token_from_credentials(credentials: ResolvedCredentials) -> anyhow::Result<String> {
    let token = credentials
        .env_vars
        .get("GITHUB_TOKEN")
        .map(String::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    cleanup_credential_temp_files(&credentials.temp_files)?;
    if !credentials.file_mounts.is_empty()
        || !credentials.temp_files.is_empty()
        || credentials.env_vars.len() != 1
    {
        anyhow::bail!(
            "GitHub action credential provider must only return one token environment variable"
        );
    }
    token.context("GitHub action credential provider returned no GITHUB_TOKEN")
}

fn cleanup_credential_temp_files(temp_files: &[PathBuf]) -> anyhow::Result<()> {
    for path in temp_files {
        std::fs::remove_file(path)
            .with_context(|| format!("remove temporary credential file {}", path.display()))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_github_action_with<H: GithubHttpTransport, G: GitCommandRunner>(
    action: &protocol::GithubAction,
    runtime: &protocol::CanonicalRuntimeSpec,
    workspace: &Path,
    work_item_context: &serde_json::Value,
    token: &str,
    transport: &H,
    git: &G,
) -> anyhow::Result<serde_json::Value> {
    let capability = runtime
        .github_capabilities
        .as_slice()
        .first()
        .filter(|_| runtime.github_capabilities.len() == 1)
        .context("managed GitHub action requires exactly one capability")?;
    let branch = capability
        .branch
        .as_deref()
        .context("GitHub action has no branch constraint")?;
    validate_git_ref(branch)?;
    let base_branch = capability
        .base_branch
        .as_deref()
        .context("GitHub action has no base branch constraint")?;
    validate_git_ref(base_branch)?;
    let api_base =
        protocol::canonical_github_api_base_for_host(&capability.host, &capability.api_base_url)
            .context("GitHub action API base URL is invalid")?;
    let repository_url = format!("{api_base}/repos/{}", capability.repository);

    match action {
        protocol::GithubAction::PushHead => {
            let output = git
                .run(GitCommandRequest {
                    workspace: workspace.to_path_buf(),
                    args: vec!["rev-parse".to_string(), "HEAD".to_string()],
                    env: HashMap::new(),
                })
                .await?;
            if !output.success {
                anyhow::bail!("failed to resolve exact repository HEAD");
            }
            let head_sha = String::from_utf8(output.stdout)?.trim().to_string();
            validate_git_sha(&head_sha)?;
            let askpass = TemporaryCredentialFile::create(
                b"#!/bin/sh\ncase \"$1\" in *Username*) printf x-access-token;; *Password*) printf %s \"$AGENT_HUB_GITHUB_TOKEN\";; *) exit 1;; esac\n",
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(askpass.path(), std::fs::Permissions::from_mode(0o700))?;
            }
            let remote = format!("https://{}/{}.git", capability.host, capability.repository);
            let refspec = format!("HEAD:refs/heads/{branch}");
            let push = git
                .run(GitCommandRequest {
                    workspace: workspace.to_path_buf(),
                    args: vec![
                        "-c".to_string(),
                        "core.hooksPath=/dev/null".to_string(),
                        "-c".to_string(),
                        "credential.helper=".to_string(),
                        "-c".to_string(),
                        format!("url.{remote}.insteadOf={remote}"),
                        "push".to_string(),
                        "--".to_string(),
                        remote,
                        refspec,
                    ],
                    env: HashMap::from([
                        (
                            "GIT_ASKPASS".to_string(),
                            askpass.path().to_string_lossy().into_owned(),
                        ),
                        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
                        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
                        ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
                        ("AGENT_HUB_GITHUB_TOKEN".to_string(), token.to_string()),
                    ]),
                })
                .await;
            askpass.cleanup()?;
            let push = push?;
            if !push.success {
                anyhow::bail!("exact GitHub branch push failed");
            }
            Ok(serde_json::json!({
                "status": "completed",
                "next_event": "job_succeeded",
                "summary": format!("Pushed exact HEAD to {branch}"),
                "head_sha": head_sha,
                "branch": branch,
            }))
        }
        protocol::GithubAction::ReadOrCreatePullRequest => {
            let owner = capability
                .repository
                .split_once('/')
                .map(|(owner, _)| owner)
                .context("invalid GitHub repository identity")?;
            let list = send_github_json(
                transport,
                token,
                GithubHttpRequest {
                    method: reqwest::Method::GET,
                    url: format!("{repository_url}/pulls"),
                    query: vec![
                        ("state".to_string(), "open".to_string()),
                        ("head".to_string(), format!("{owner}:{branch}")),
                    ],
                    body: None,
                },
            )
            .await?;
            let list = list
                .as_array()
                .context("GitHub pull request list was not an array")?;
            let matches = list
                .iter()
                .filter(|pull| validate_pull_identity(pull, capability, None).is_ok())
                .collect::<Vec<_>>();
            if matches.len() > 1 {
                anyhow::bail!("GitHub returned ambiguous exact pull request matches");
            }
            let pull = if let Some(pull) = matches.first() {
                (*pull).clone()
            } else {
                let title = work_item_context
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("work item title is required to create a pull request")?;
                send_github_json(
                    transport,
                    token,
                    GithubHttpRequest {
                        method: reqwest::Method::POST,
                        url: format!("{repository_url}/pulls"),
                        query: Vec::new(),
                        body: Some(serde_json::json!({
                        "title": title,
                        "body": work_item_context.get("description").and_then(serde_json::Value::as_str).unwrap_or(""),
                        "head": branch,
                        "base": base_branch,
                        "draft": true,
                        })),
                    },
                )
                .await?
            };
            validate_pull_identity(&pull, capability, None)?;
            github_pull_output(&pull, branch, base_branch)
        }
        protocol::GithubAction::MarkPullRequestReady => {
            let pull = fetch_exact_pull(transport, token, capability, &repository_url).await?;
            if pull.get("draft").and_then(serde_json::Value::as_bool) == Some(false) {
                return Ok(github_action_success("Pull request is already ready"));
            }
            let node_id = pull
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                .context("GitHub pull request has no node id")?;
            let graphql_url = github_graphql_url(&api_base)?;
            let response = send_github_json(
                transport,
                token,
                GithubHttpRequest {
                    method: reqwest::Method::POST,
                    url: graphql_url,
                    query: Vec::new(),
                    body: Some(serde_json::json!({
                        "query": "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{isDraft}}}",
                        "variables": {"id": node_id},
                    })),
                },
            )
            .await?;
            if response.get("errors").is_some() {
                anyhow::bail!("GitHub rejected mark-ready action");
            }
            Ok(github_action_success("Pull request marked ready"))
        }
        protocol::GithubAction::RequestReview => {
            let pull = fetch_exact_pull(transport, token, capability, &repository_url).await?;
            let reviewer = work_item_context
                .get("owner_github_login")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .context("exact reviewer login is required")?;
            validate_github_login(reviewer)?;
            if pull
                .get("requested_reviewers")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|reviewers| {
                    reviewers.iter().any(|requested| {
                        requested.get("login").and_then(serde_json::Value::as_str) == Some(reviewer)
                    })
                })
            {
                return Ok(github_action_success(
                    "Exact pull request reviewer is already requested",
                ));
            }
            let number = pull
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .unwrap();
            send_github_json(
                transport,
                token,
                GithubHttpRequest {
                    method: reqwest::Method::POST,
                    url: format!("{repository_url}/pulls/{number}/requested_reviewers"),
                    query: Vec::new(),
                    body: Some(serde_json::json!({"reviewers": [reviewer]})),
                },
            )
            .await?;
            Ok(github_action_success(
                "Requested exact pull request reviewer",
            ))
        }
        protocol::GithubAction::MergePullRequest => {
            let pull = fetch_exact_pull(transport, token, capability, &repository_url).await?;
            let number = pull
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .unwrap();
            let expected_sha = capability
                .expected_head_sha
                .as_deref()
                .context("merge capability has no expected head SHA")?;
            validate_git_sha(expected_sha)?;
            if pull
                .pointer("/head/sha")
                .and_then(serde_json::Value::as_str)
                != Some(expected_sha)
            {
                anyhow::bail!("pull request head SHA no longer matches authorization");
            }
            if pull.get("merged").and_then(serde_json::Value::as_bool) == Some(true) {
                return Ok(github_action_success(
                    "Merge already observed by GitHub; awaiting authoritative workflow observation",
                ));
            }
            let response = send_github_json(
                transport,
                token,
                GithubHttpRequest {
                    method: reqwest::Method::PUT,
                    url: format!("{repository_url}/pulls/{number}/merge"),
                    query: Vec::new(),
                    body: Some(serde_json::json!({
                        "sha": expected_sha,
                        "merge_method": "merge"
                    })),
                },
            )
            .await?;
            if response.get("merged").and_then(serde_json::Value::as_bool) != Some(true) {
                anyhow::bail!("GitHub did not accept the exact merge action");
            }
            Ok(github_action_success(
                "Merge command accepted; awaiting authoritative GitHub observation",
            ))
        }
    }
}

async fn send_github_json<H: GithubHttpTransport>(
    transport: &H,
    token: &str,
    request: GithubHttpRequest,
) -> anyhow::Result<serde_json::Value> {
    let response = transport.send(request, token).await?;
    if !response.status.is_success() {
        anyhow::bail!("GitHub request failed with HTTP {}", response.status);
    }
    Ok(response.body)
}

async fn fetch_exact_pull<H: GithubHttpTransport>(
    transport: &H,
    token: &str,
    capability: &protocol::GithubCapability,
    repository_url: &str,
) -> anyhow::Result<serde_json::Value> {
    let number = capability
        .pull_request
        .context("GitHub action requires an exact pull request number")?;
    let pull = send_github_json(
        transport,
        token,
        GithubHttpRequest {
            method: reqwest::Method::GET,
            url: format!("{repository_url}/pulls/{number}"),
            query: Vec::new(),
            body: None,
        },
    )
    .await?;
    validate_pull_identity(&pull, capability, Some(number))?;
    Ok(pull)
}

fn validate_pull_identity(
    pull: &serde_json::Value,
    capability: &protocol::GithubCapability,
    exact_number: Option<u64>,
) -> anyhow::Result<()> {
    let number = pull
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .context("GitHub pull request has no number")?;
    let parsed_url = pull
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .and_then(protocol::parse_github_pull_request_url)
        .context("GitHub pull request URL is invalid")?;
    let response_repository = format!(
        "{}/{}",
        parsed_url.repository.owner, parsed_url.repository.repository
    );
    if exact_number.is_some_and(|expected| expected != number)
        || parsed_url.number != number
        || parsed_url.repository.host != capability.host
        || response_repository != capability.repository
        || pull
            .pointer("/head/ref")
            .and_then(serde_json::Value::as_str)
            != capability.head_branch.as_deref()
        || pull
            .pointer("/base/ref")
            .and_then(serde_json::Value::as_str)
            != capability.base_branch.as_deref()
        || pull
            .pointer("/head/repo/full_name")
            .and_then(serde_json::Value::as_str)
            != Some(capability.repository.as_str())
        || pull
            .pointer("/base/repo/full_name")
            .and_then(serde_json::Value::as_str)
            != Some(capability.repository.as_str())
    {
        anyhow::bail!("pull request identity or branch constraints no longer match authorization");
    }
    Ok(())
}

fn github_graphql_url(api_base_url: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(api_base_url)?;
    let path = url.path().trim_end_matches('/');
    let graphql_path = if let Some(prefix) = path.strip_suffix("/v3") {
        format!("{prefix}/graphql")
    } else {
        format!("{path}/graphql")
    };
    url.set_path(&graphql_path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn github_pull_output(
    pull: &serde_json::Value,
    branch: &str,
    base_branch: &str,
) -> anyhow::Result<serde_json::Value> {
    let number = pull
        .get("number")
        .and_then(serde_json::Value::as_u64)
        .context("pull request has no number")?;
    let url = pull
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .context("pull request has no URL")?;
    Ok(serde_json::json!({
        "status": "completed",
        "next_event": "job_succeeded",
        "summary": format!("Draft pull request #{number} is available"),
        "github_pr": {
            "number": number,
            "url": url,
            "state": "open",
            "is_draft": pull.get("draft").and_then(serde_json::Value::as_bool).unwrap_or(true),
            "head_ref_name": branch,
            "base_ref_name": base_branch,
        }
    }))
}

fn github_action_success(summary: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "completed",
        "next_event": "job_succeeded",
        "summary": summary,
    })
}

fn validate_git_ref(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        anyhow::bail!("invalid exact Git ref constraint");
    }
    Ok(())
}

fn validate_git_sha(value: &str) -> anyhow::Result<()> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid exact Git commit SHA");
    }
    Ok(())
}

fn validate_github_login(value: &str) -> anyhow::Result<()> {
    if value.len() > 39
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        anyhow::bail!("invalid exact GitHub reviewer login");
    }
    Ok(())
}

#[cfg(test)]
mod github_action_tests {
    use super::*;
    use crate::credential_provider::CredentialProvider;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TOKEN: &str = "test-token-that-must-not-leak";
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    struct FakeHttp {
        expected_token: String,
        requests: StdMutex<Vec<GithubHttpRequest>>,
        responses: StdMutex<VecDeque<GithubHttpResponse>>,
    }

    impl FakeHttp {
        fn new(responses: impl IntoIterator<Item = serde_json::Value>) -> Self {
            Self {
                expected_token: TOKEN.to_string(),
                requests: StdMutex::new(Vec::new()),
                responses: StdMutex::new(
                    responses
                        .into_iter()
                        .map(|body| GithubHttpResponse {
                            status: reqwest::StatusCode::OK,
                            body,
                        })
                        .collect(),
                ),
            }
        }

        fn with_status(status: reqwest::StatusCode) -> Self {
            Self {
                expected_token: TOKEN.to_string(),
                requests: StdMutex::new(Vec::new()),
                responses: StdMutex::new(VecDeque::from([GithubHttpResponse {
                    status,
                    body: serde_json::json!({"message": TOKEN}),
                }])),
            }
        }

        fn requests(&self) -> Vec<GithubHttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl GithubHttpTransport for FakeHttp {
        async fn send(
            &self,
            request: GithubHttpRequest,
            token: &str,
        ) -> anyhow::Result<GithubHttpResponse> {
            assert_eq!(token, self.expected_token);
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("unexpected GitHub HTTP request")
        }
    }

    struct FakeGit {
        requests: StdMutex<Vec<GitCommandRequest>>,
        askpass_existed: StdMutex<Vec<bool>>,
        outputs: StdMutex<VecDeque<Result<GitCommandOutput, &'static str>>>,
    }

    impl FakeGit {
        fn new(outputs: impl IntoIterator<Item = Result<GitCommandOutput, &'static str>>) -> Self {
            Self {
                requests: StdMutex::new(Vec::new()),
                askpass_existed: StdMutex::new(Vec::new()),
                outputs: StdMutex::new(outputs.into_iter().collect()),
            }
        }

        fn requests(&self) -> Vec<GitCommandRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl GitCommandRunner for FakeGit {
        async fn run(&self, request: GitCommandRequest) -> anyhow::Result<GitCommandOutput> {
            if let Some(path) = request.env.get("GIT_ASKPASS") {
                self.askpass_existed
                    .lock()
                    .unwrap()
                    .push(Path::new(path).is_file());
            }
            self.requests.lock().unwrap().push(request);
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .context("unexpected git command")?
                .map_err(anyhow::Error::msg)
        }
    }

    fn capability(
        host: &str,
        operations: Vec<protocol::GithubOperation>,
    ) -> protocol::GithubCapability {
        protocol::GithubCapability {
            host: host.to_string(),
            api_base_url: if host == "github.com" {
                "https://api.github.com".to_string()
            } else {
                format!("https://{host}/api/v3")
            },
            repository: "reddit/agent-hub".to_string(),
            operations,
            branch: Some("agent-hub/task".to_string()),
            head_branch: Some("agent-hub/task".to_string()),
            base_branch: Some("main".to_string()),
            pull_request: Some(42),
            expected_head_sha: Some(SHA.to_string()),
            credential_provider: github_read_write_provider(host),
        }
    }

    fn runtime(capability: protocol::GithubCapability) -> protocol::CanonicalRuntimeSpec {
        let environment = protocol::canonical_environment_for_execution(
            ExecutionMode::Docker,
            Some("image".to_string()),
            None,
        );
        protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            permissions: protocol::canonical_permissions_for_environment(&environment),
            environment,
            approval_rules: None,
            repos: Vec::new(),
            github_capabilities: vec![capability],
        }
    }

    fn repo(host: &str) -> protocol::RepoSource {
        protocol::RepoSource {
            repo_url: format!("https://{host}/reddit/agent-hub.git"),
            base_ref: "origin/main".to_string(),
            branch_name: Some("agent-hub/task".to_string()),
        }
    }

    fn pull(host: &str) -> serde_json::Value {
        serde_json::json!({
            "number": 42,
            "html_url": format!("https://{host}/reddit/agent-hub/pull/42"),
            "node_id": "PR_node_42",
            "draft": true,
            "head": {
                "ref": "agent-hub/task",
                "sha": SHA,
                "repo": {"full_name": "reddit/agent-hub"}
            },
            "base": {
                "ref": "main",
                "repo": {"full_name": "reddit/agent-hub"}
            },
            "requested_reviewers": []
        })
    }

    fn no_git() -> FakeGit {
        FakeGit::new([])
    }

    #[test]
    fn managed_github_capability_requires_exact_signed_constraints() {
        let operations = vec![
            protocol::GithubOperation::ReadPullRequest,
            protocol::GithubOperation::MergePullRequest,
        ];
        let base = capability("github.com", operations);
        let base_runtime = runtime(base.clone());
        assert!(
            validate_github_action_capability(
                &protocol::GithubAction::MergePullRequest,
                &base_runtime,
                Some(&repo("github.com")),
            )
            .is_ok()
        );

        let mut invalid_runtimes = vec![protocol::CanonicalRuntimeSpec {
            github_capabilities: Vec::new(),
            ..base_runtime.clone()
        }];
        let mut two = base_runtime.clone();
        two.github_capabilities.push(base.clone());
        invalid_runtimes.push(two);
        for mutate in [
            |value: &mut protocol::GithubCapability| value.host = "other.example".to_string(),
            |value: &mut protocol::GithubCapability| {
                value.api_base_url = "http://api.github.com".to_string()
            },
            |value: &mut protocol::GithubCapability| value.repository = "reddit/other".to_string(),
            |value: &mut protocol::GithubCapability| value.branch = Some("other".to_string()),
            |value: &mut protocol::GithubCapability| value.head_branch = Some("other".to_string()),
            |value: &mut protocol::GithubCapability| value.base_branch = Some("other".to_string()),
            |value: &mut protocol::GithubCapability| value.pull_request = None,
            |value: &mut protocol::GithubCapability| value.expected_head_sha = None,
            |value: &mut protocol::GithubCapability| {
                value.credential_provider = "github:github.com:read_only".to_string()
            },
            |value: &mut protocol::GithubCapability| {
                value.operations.push(protocol::GithubOperation::PushBranch)
            },
        ] {
            let mut invalid = base.clone();
            mutate(&mut invalid);
            invalid_runtimes.push(runtime(invalid));
        }
        for invalid in invalid_runtimes {
            assert!(
                validate_github_action_capability(
                    &protocol::GithubAction::MergePullRequest,
                    &invalid,
                    Some(&repo("github.com")),
                )
                .is_err()
            );
        }
        assert!(
            validate_github_action_capability(
                &protocol::GithubAction::RequestReview,
                &base_runtime,
                Some(&repo("github.com")),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn github_tokens_are_host_specific_and_fail_closed() {
        let mut config = LocalAgentConfig::default();
        config.github_hosts = vec!["github.com".to_string(), "github.snooguts.net".to_string()];
        config.github_api_base_urls = HashMap::from([
            (
                "github.com".to_string(),
                "https://api.github.com".to_string(),
            ),
            (
                "github.snooguts.net".to_string(),
                "https://github.snooguts.net/api/v3".to_string(),
            ),
        ]);
        for (host, token) in [
            ("github.com", "dotcom-token"),
            ("github.snooguts.net", "ghe-token"),
        ] {
            config.credential_providers.insert(
                github_read_write_provider(host),
                CredentialProvider::Env {
                    name: github_read_write_provider(host),
                    vars: HashMap::from([("GITHUB_TOKEN".to_string(), token.to_string())]),
                },
            );
            let capability = capability(host, vec![protocol::GithubOperation::PushBranch]);
            assert_eq!(
                resolve_github_action_token(&config, &capability)
                    .await
                    .expect("host token"),
                token
            );
        }

        let missing = capability(
            "missing.example",
            vec![protocol::GithubOperation::PushBranch],
        );
        assert!(
            resolve_github_action_token(&config, &missing)
                .await
                .is_err()
        );
        let mut mismatch = capability("github.com", vec![protocol::GithubOperation::PushBranch]);
        mismatch.credential_provider = github_read_write_provider("github.snooguts.net");
        assert!(
            resolve_github_action_token(&config, &mismatch)
                .await
                .is_err()
        );
        let mut mismatched_api =
            capability("github.com", vec![protocol::GithubOperation::PushBranch]);
        mismatched_api.api_base_url = "https://ghe-api.example.com/custom".to_string();
        assert!(
            resolve_github_action_token(&config, &mismatched_api)
                .await
                .is_err()
        );
        config.credential_providers.insert(
            github_read_write_provider("github.com"),
            CredentialProvider::Env {
                name: github_read_write_provider("github.com"),
                vars: HashMap::from([("GITHUB_TOKEN".to_string(), "  ".to_string())]),
            },
        );
        assert!(
            resolve_github_action_token(
                &config,
                &capability("github.com", vec![protocol::GithubOperation::PushBranch]),
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn github_tokens_and_host_config_are_rejected_for_general_jobs() {
        for name in [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_HOST",
            "GH_CONFIG_DIR",
            "AGENT_HUB_GITHUB_TOKEN",
        ] {
            let credentials = ResolvedCredentials {
                env_vars: HashMap::from([(name.to_string(), TOKEN.to_string())]),
                ..Default::default()
            };
            let error = validate_general_job_credentials(&credentials).unwrap_err();
            assert!(!format!("{error:#}").contains(TOKEN));
        }
    }

    #[tokio::test]
    async fn read_or_create_pull_request_is_idempotent_and_creates_exact_draft() {
        let mut capability = capability(
            "github.com",
            vec![
                protocol::GithubOperation::ReadPullRequest,
                protocol::GithubOperation::CreatePullRequest,
            ],
        );
        capability.pull_request = None;
        let runtime = runtime(capability);
        let context = serde_json::json!({"title":"Task title", "description":"Task body"});

        let existing = FakeHttp::new([serde_json::json!([pull("github.com")])]);
        let output = execute_github_action_with(
            &protocol::GithubAction::ReadOrCreatePullRequest,
            &runtime,
            Path::new("/workspace"),
            &context,
            TOKEN,
            &existing,
            &no_git(),
        )
        .await
        .expect("existing pull request");
        assert_eq!(output["github_pr"]["number"], 42);
        assert_eq!(existing.requests().len(), 1);

        let mut wrong_base = pull("github.com");
        wrong_base["base"]["ref"] = serde_json::json!("other");
        let filtered = FakeHttp::new([serde_json::json!([wrong_base, pull("github.com")])]);
        execute_github_action_with(
            &protocol::GithubAction::ReadOrCreatePullRequest,
            &runtime,
            Path::new("/workspace"),
            &context,
            TOKEN,
            &filtered,
            &no_git(),
        )
        .await
        .expect("one exact pull request after filtering");
        assert_eq!(filtered.requests().len(), 1);

        let ambiguous =
            FakeHttp::new([serde_json::json!([pull("github.com"), pull("github.com")])]);
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::ReadOrCreatePullRequest,
                &runtime,
                Path::new("/workspace"),
                &context,
                TOKEN,
                &ambiguous,
                &no_git(),
            )
            .await
            .is_err()
        );
        assert_eq!(ambiguous.requests().len(), 1);

        let absent = FakeHttp::new([serde_json::json!([]), pull("github.com")]);
        execute_github_action_with(
            &protocol::GithubAction::ReadOrCreatePullRequest,
            &runtime,
            Path::new("/workspace"),
            &context,
            TOKEN,
            &absent,
            &no_git(),
        )
        .await
        .expect("created pull request");
        let requests = absent.requests();
        assert_eq!(requests[0].method, reqwest::Method::GET);
        assert_eq!(
            requests[0].query,
            vec![
                ("state".to_string(), "open".to_string()),
                ("head".to_string(), "reddit:agent-hub/task".to_string()),
            ]
        );
        assert_eq!(requests[1].method, reqwest::Method::POST);
        assert_eq!(
            requests[1].body,
            Some(serde_json::json!({
                "title":"Task title",
                "body":"Task body",
                "head":"agent-hub/task",
                "base":"main",
                "draft":true
            }))
        );

        let no_title = FakeHttp::new([serde_json::json!([])]);
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::ReadOrCreatePullRequest,
                &runtime,
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &no_title,
                &no_git(),
            )
            .await
            .is_err()
        );
        assert_eq!(no_title.requests().len(), 1);
    }

    #[tokio::test]
    async fn pull_request_identity_mismatches_fail_before_mutation() {
        let runtime = runtime(capability(
            "github.com",
            vec![
                protocol::GithubOperation::ReadPullRequest,
                protocol::GithubOperation::RequestReview,
            ],
        ));
        for pointer in [
            "/number",
            "/html_url",
            "/head/ref",
            "/base/ref",
            "/head/repo/full_name",
            "/base/repo/full_name",
        ] {
            let mut mismatched = pull("github.com");
            *mismatched.pointer_mut(pointer).unwrap() = match pointer {
                "/number" => serde_json::json!(43),
                _ => serde_json::json!("mismatch"),
            };
            let http = FakeHttp::new([mismatched]);
            assert!(
                execute_github_action_with(
                    &protocol::GithubAction::RequestReview,
                    &runtime,
                    Path::new("/workspace"),
                    &serde_json::json!({"owner_github_login":"octocat"}),
                    TOKEN,
                    &http,
                    &no_git(),
                )
                .await
                .is_err()
            );
            assert_eq!(http.requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn mark_ready_uses_exact_node_and_routes_graphql_by_host() {
        for (host, api_base, graphql_url) in [
            (
                "github.com",
                "https://api.github.com",
                "https://api.github.com/graphql",
            ),
            (
                "github.snooguts.net",
                "https://github.snooguts.net/api/v3",
                "https://github.snooguts.net/api/graphql",
            ),
            (
                "ghe.example.com",
                "https://ghe.example.com/custom/api/v3",
                "https://ghe.example.com/custom/api/graphql",
            ),
        ] {
            let http = FakeHttp::new([pull(host), serde_json::json!({"data":{}})]);
            let mut capability = capability(
                host,
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::MarkPullRequestReady,
                ],
            );
            capability.api_base_url = api_base.to_string();
            execute_github_action_with(
                &protocol::GithubAction::MarkPullRequestReady,
                &runtime(capability),
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &http,
                &no_git(),
            )
            .await
            .expect("mark ready");
            let requests = http.requests();
            assert_eq!(
                requests[0].url,
                format!("{api_base}/repos/reddit/agent-hub/pulls/42")
            );
            assert_eq!(requests[1].url, graphql_url);
            assert_eq!(
                requests[1].body.as_ref().unwrap()["variables"]["id"],
                "PR_node_42"
            );
        }

        let rejected = FakeHttp::new([
            pull("github.com"),
            serde_json::json!({"errors":[{"message":TOKEN}]}),
        ]);
        let error = execute_github_action_with(
            &protocol::GithubAction::MarkPullRequestReady,
            &runtime(capability(
                "github.com",
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::MarkPullRequestReady,
                ],
            )),
            Path::new("/workspace"),
            &serde_json::json!({}),
            TOKEN,
            &rejected,
            &no_git(),
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:#}").contains(TOKEN));
    }

    #[tokio::test]
    async fn reviewer_request_is_exact_and_invalid_login_never_mutates() {
        let runtime = runtime(capability(
            "github.com",
            vec![
                protocol::GithubOperation::ReadPullRequest,
                protocol::GithubOperation::RequestReview,
            ],
        ));
        let http = FakeHttp::new([pull("github.com"), serde_json::Value::Null]);
        execute_github_action_with(
            &protocol::GithubAction::RequestReview,
            &runtime,
            Path::new("/workspace"),
            &serde_json::json!({"owner_github_login":"octocat"}),
            TOKEN,
            &http,
            &no_git(),
        )
        .await
        .expect("request review");
        let requests = http.requests();
        assert_eq!(
            requests[1].url,
            "https://api.github.com/repos/reddit/agent-hub/pulls/42/requested_reviewers"
        );
        assert_eq!(
            requests[1].body,
            Some(serde_json::json!({"reviewers":["octocat"]}))
        );

        let invalid = FakeHttp::new([pull("github.com")]);
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::RequestReview,
                &runtime,
                Path::new("/workspace"),
                &serde_json::json!({"owner_github_login":"invalid login"}),
                TOKEN,
                &invalid,
                &no_git(),
            )
            .await
            .is_err()
        );
        assert_eq!(invalid.requests().len(), 1);
    }

    #[tokio::test]
    async fn merge_checks_exact_identity_and_sha_before_mutation() {
        let runtime = runtime(capability(
            "github.com",
            vec![
                protocol::GithubOperation::ReadPullRequest,
                protocol::GithubOperation::MergePullRequest,
            ],
        ));
        let http = FakeHttp::new([pull("github.com"), serde_json::json!({"merged":true})]);
        let output = execute_github_action_with(
            &protocol::GithubAction::MergePullRequest,
            &runtime,
            Path::new("/workspace"),
            &serde_json::json!({}),
            TOKEN,
            &http,
            &no_git(),
        )
        .await
        .expect("merge accepted");
        assert_eq!(output["status"], "completed");
        assert_eq!(output["next_event"], "job_succeeded");
        assert!(output["summary"].as_str().unwrap().contains("awaiting"));
        let requests = http.requests();
        assert_eq!(requests[1].method, reqwest::Method::PUT);
        assert_eq!(
            requests[1].body,
            Some(serde_json::json!({"sha":SHA,"merge_method":"merge"}))
        );

        let mut changed = pull("github.com");
        changed["head"]["sha"] =
            serde_json::Value::String("abcdef0123456789abcdef0123456789abcdef01".to_string());
        let mismatch = FakeHttp::new([changed]);
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::MergePullRequest,
                &runtime,
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &mismatch,
                &no_git(),
            )
            .await
            .is_err()
        );
        assert_eq!(mismatch.requests().len(), 1);
    }

    #[tokio::test]
    async fn managed_github_retries_are_idempotent_at_exact_observed_state() {
        let mut ready = pull("github.com");
        ready["draft"] = serde_json::Value::Bool(false);
        let ready_http = FakeHttp::new([ready]);
        execute_github_action_with(
            &protocol::GithubAction::MarkPullRequestReady,
            &runtime(capability(
                "github.com",
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::MarkPullRequestReady,
                ],
            )),
            Path::new("/workspace"),
            &serde_json::json!({}),
            TOKEN,
            &ready_http,
            &no_git(),
        )
        .await
        .expect("already ready");
        assert_eq!(ready_http.requests().len(), 1);

        let mut reviewed = pull("github.com");
        reviewed["requested_reviewers"] = serde_json::json!([{"login":"octocat"}]);
        let review_http = FakeHttp::new([reviewed]);
        execute_github_action_with(
            &protocol::GithubAction::RequestReview,
            &runtime(capability(
                "github.com",
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::RequestReview,
                ],
            )),
            Path::new("/workspace"),
            &serde_json::json!({"owner_github_login":"octocat"}),
            TOKEN,
            &review_http,
            &no_git(),
        )
        .await
        .expect("reviewer already requested");
        assert_eq!(review_http.requests().len(), 1);

        let mut merged = pull("github.com");
        merged["merged"] = serde_json::Value::Bool(true);
        let merge_http = FakeHttp::new([merged]);
        let output = execute_github_action_with(
            &protocol::GithubAction::MergePullRequest,
            &runtime(capability(
                "github.com",
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::MergePullRequest,
                ],
            )),
            Path::new("/workspace"),
            &serde_json::json!({}),
            TOKEN,
            &merge_http,
            &no_git(),
        )
        .await
        .expect("already merged");
        assert!(output["summary"].as_str().unwrap().contains("awaiting"));
        assert_eq!(merge_http.requests().len(), 1);
    }

    #[tokio::test]
    async fn push_uses_exact_remote_ref_and_cleans_askpass_on_success_and_failure() {
        for push_output in [
            Ok(GitCommandOutput {
                success: true,
                stdout: Vec::new(),
            }),
            Ok(GitCommandOutput {
                success: false,
                stdout: TOKEN.as_bytes().to_vec(),
            }),
            Err("git runner failed"),
        ] {
            let git = FakeGit::new([
                Ok(GitCommandOutput {
                    success: true,
                    stdout: format!("{SHA}\n").into_bytes(),
                }),
                push_output,
            ]);
            let result = execute_github_action_with(
                &protocol::GithubAction::PushHead,
                &runtime(capability(
                    "github.snooguts.net",
                    vec![protocol::GithubOperation::PushBranch],
                )),
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &FakeHttp::new([]),
                &git,
            )
            .await;
            let requests = git.requests();
            assert_eq!(requests[0].args, ["rev-parse", "HEAD"]);
            assert_eq!(
                &requests[1].args[requests[1].args.len() - 2..],
                [
                    "https://github.snooguts.net/reddit/agent-hub.git",
                    "HEAD:refs/heads/agent-hub/task"
                ]
            );
            assert!(
                requests[1]
                    .args
                    .contains(&"core.hooksPath=/dev/null".to_string())
            );
            assert!(requests[1].args.contains(&"credential.helper=".to_string()));
            assert_eq!(
                requests[1].env.get("GIT_CONFIG_GLOBAL").unwrap(),
                "/dev/null"
            );
            let askpass = requests[1].env.get("GIT_ASKPASS").unwrap();
            assert!(!Path::new(askpass).exists());
            assert_eq!(*git.askpass_existed.lock().unwrap(), vec![true]);
            if let Err(error) = result {
                assert!(!format!("{error:#}").contains(TOKEN));
            }
        }

        let invalid_sha = FakeGit::new([Ok(GitCommandOutput {
            success: true,
            stdout: b"not-a-sha".to_vec(),
        })]);
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::PushHead,
                &runtime(capability(
                    "github.com",
                    vec![protocol::GithubOperation::PushBranch],
                )),
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &FakeHttp::new([]),
                &invalid_sha,
            )
            .await
            .is_err()
        );
        assert_eq!(invalid_sha.requests().len(), 1);

        let mut invalid_ref = capability("github.com", vec![protocol::GithubOperation::PushBranch]);
        invalid_ref.branch = Some("../redirect".to_string());
        let no_commands = no_git();
        assert!(
            execute_github_action_with(
                &protocol::GithubAction::PushHead,
                &runtime(invalid_ref),
                Path::new("/workspace"),
                &serde_json::json!({}),
                TOKEN,
                &FakeHttp::new([]),
                &no_commands,
            )
            .await
            .is_err()
        );
        assert!(no_commands.requests().is_empty());
    }

    #[tokio::test]
    async fn github_http_errors_and_outputs_never_contain_token() {
        let http = FakeHttp::with_status(reqwest::StatusCode::UNAUTHORIZED);
        let error = execute_github_action_with(
            &protocol::GithubAction::RequestReview,
            &runtime(capability(
                "github.com",
                vec![
                    protocol::GithubOperation::ReadPullRequest,
                    protocol::GithubOperation::RequestReview,
                ],
            )),
            Path::new("/workspace"),
            &serde_json::json!({"owner_github_login":"octocat"}),
            TOKEN,
            &http,
            &no_git(),
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:#}").contains(TOKEN));

        let temp = std::env::temp_dir().join(format!("github-cred-{}", uuid::Uuid::new_v4()));
        std::fs::write(&temp, TOKEN).unwrap();
        let credentials = ResolvedCredentials {
            env_vars: HashMap::from([("GITHUB_TOKEN".to_string(), TOKEN.to_string())]),
            file_mounts: vec![(
                temp.to_string_lossy().into_owned(),
                "/credential".to_string(),
                true,
            )],
            temp_files: vec![temp.clone()],
        };
        let error = github_token_from_credentials(credentials).unwrap_err();
        assert!(!format!("{error:#}").contains(TOKEN));
        assert!(!temp.exists());
    }

    #[tokio::test]
    async fn authorized_lifecycle_cleans_up_on_success_errors_timeout_and_cancellation() {
        async fn finalized<T>(
            result: anyhow::Result<T>,
            cleanup_count: Arc<AtomicUsize>,
        ) -> anyhow::Result<T> {
            finalize_required_cleanup(result, async move {
                cleanup_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
        }

        for result in [
            Ok("success"),
            Err(anyhow::anyhow!("HTTP error")),
            Err(anyhow::anyhow!("git error")),
        ] {
            let cleanup_count = Arc::new(AtomicUsize::new(0));
            let _ = finalized(result, Arc::clone(&cleanup_count)).await;
            assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
        }

        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let timed_out = run_authorized_github_operation(
            std::future::pending::<anyhow::Result<()>>(),
            Some(0),
            cancel_rx,
        )
        .await;
        let timeout_cleanup = Arc::new(AtomicUsize::new(0));
        assert!(
            finalized(timed_out, Arc::clone(&timeout_cleanup))
                .await
                .is_err()
        );
        assert_eq!(timeout_cleanup.load(Ordering::SeqCst), 1);

        struct DropSignal(Arc<AtomicUsize>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let operation = {
            let guard = DropSignal(Arc::clone(&dropped));
            async move {
                let _guard = guard;
                std::future::pending::<anyhow::Result<()>>().await
            }
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        cancel_tx.send(true).unwrap();
        let cancelled = run_authorized_github_operation(operation, Some(60), cancel_rx).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let cancel_cleanup = Arc::new(AtomicUsize::new(0));
        assert!(
            finalized(cancelled, Arc::clone(&cancel_cleanup))
                .await
                .is_err()
        );
        assert_eq!(cancel_cleanup.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_github_credential_command_is_timed_out_and_terminated() {
        let pid_path =
            std::env::temp_dir().join(format!("credential-pid-{}", uuid::Uuid::new_v4()));
        let mut config = LocalAgentConfig::default();
        config.github_hosts = vec!["github.com".to_string()];
        config.github_api_base_urls = HashMap::from([(
            "github.com".to_string(),
            "https://api.github.com".to_string(),
        )]);
        config.credential_providers.insert(
            github_read_write_provider("github.com"),
            CredentialProvider::Command {
                name: github_read_write_provider("github.com"),
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!("echo $$ > '{}'; exec sleep 30", pid_path.display()),
                ],
                capture_env: Some("GITHUB_TOKEN".to_string()),
                capture_file: None,
            },
        );
        let capability = capability("github.com", vec![protocol::GithubOperation::PushBranch]);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let started = Instant::now();
        let result = run_authorized_github_operation(
            resolve_github_action_token(&config, &capability),
            Some(1),
            cancel_rx,
        )
        .await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid = std::fs::read_to_string(&pid_path).expect("credential command started");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
        let _ = std::fs::remove_file(pid_path);
    }

    #[test]
    fn github_credential_shape_fails_closed() {
        for credentials in [
            ResolvedCredentials::default(),
            ResolvedCredentials {
                env_vars: HashMap::from([
                    ("GITHUB_TOKEN".to_string(), TOKEN.to_string()),
                    ("OTHER".to_string(), "value".to_string()),
                ]),
                ..Default::default()
            },
            ResolvedCredentials {
                env_vars: HashMap::from([("OTHER".to_string(), TOKEN.to_string())]),
                ..Default::default()
            },
        ] {
            let error = github_token_from_credentials(credentials).unwrap_err();
            assert!(!format!("{error:#}").contains(TOKEN));
        }
    }
}

fn authorized_execution_context(
    job: &Job,
    mut execution_context: serde_json::Value,
) -> serde_json::Value {
    if !execution_context.is_object() {
        execution_context = serde_json::json!({});
    }
    let object = execution_context
        .as_object_mut()
        .expect("execution context normalized as object");
    for key in [
        protocol::CONTEXT_AUTHORIZATION_ID_KEY,
        protocol::CONTEXT_REQUEST_DIGEST_KEY,
        protocol::CONTEXT_AUTHORIZATION_REQUEST_KEY,
    ] {
        if let Some(value) = job.context.get(key) {
            object.insert(key.to_string(), value.clone());
        }
    }
    execution_context
}

fn artifact_store_relative_path(transcript_rel_dir: &str, filename: &str) -> String {
    Path::new(transcript_rel_dir)
        .join(filename)
        .to_string_lossy()
        .to_string()
}

fn artifact_path_tail(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string()
}

#[derive(Debug, Clone, Copy)]
enum ExecutionStatus {
    Exited(i32),
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone)]
struct ResolvedRunnerSpec {
    kind: ResolvedRunnerKind,
    command: String,
    args: Vec<String>,
    file_mounts: Vec<protocol::orchestration::PlannedFileMount>,
    requires_credentials: Vec<String>,
}

#[derive(Debug, Clone)]
enum ResolvedRunnerKind {
    Generic,
    Opencode(OpencodeManagedSpec),
}

#[derive(Debug, Clone)]
struct OpencodeManagedSpec {
    max_validation_attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalAgentEgressTier {
    Llm,
    Github,
    Build,
    Full,
}

impl LocalAgentEgressTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Github => "github-only",
            Self::Build => "build",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone)]
struct LocalAgentEgressPlan {
    network_none: bool,
    tier: Option<LocalAgentEgressTier>,
}

#[derive(Debug, Clone)]
struct EnvironmentValidationTable {
    by_base: HashMap<String, Vec<String>>,
}

impl EnvironmentValidationTable {
    fn from_local_agent_config(config: &LocalAgentConfig) -> anyhow::Result<Self> {
        let tree = config.config_tree().ok_or_else(|| {
            anyhow::anyhow!("CONFIG_ROOT is required and config tree must be loaded")
        })?;

        let mut by_base = HashMap::new();
        for environment in &tree.environments {
            by_base.insert(
                environment.name.clone(),
                environment.allowed_overlays.clone(),
            );
        }
        if by_base.is_empty() {
            anyhow::bail!(
                "no environments loaded from CONFIG_ROOT; cannot validate environment selection"
            );
        }
        Ok(Self { by_base })
    }

    #[cfg(test)]
    fn for_tests() -> Self {
        Self {
            by_base: HashMap::from([("code-auto".to_string(), vec!["github".to_string()])]),
        }
    }

    fn options_string(&self) -> String {
        let mut bases = self.by_base.keys().cloned().collect::<Vec<_>>();
        bases.sort();
        bases
            .into_iter()
            .map(|base| {
                let overlays = self
                    .by_base
                    .get(&base)
                    .cloned()
                    .unwrap_or_default()
                    .join(", ");
                format!("{} (overlays: {})", base, overlays)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn overlays_for_base(&self, base: &str) -> Option<&[String]> {
        self.by_base.get(base).map(Vec::as_slice)
    }
}

fn apply_build_egress_env_defaults(
    docker_env: &mut HashMap<String, String>,
    local_agent_config: &LocalAgentConfig,
) {
    if let Some(policy_env) = local_agent_config.network_policy_env("build")
        && !policy_env.is_empty()
    {
        for (key, value) in policy_env {
            docker_env
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        return;
    }

    docker_env
        .entry("GOPROXY".to_string())
        .or_insert_with(|| "https://proxy.golang.org".to_string());
    docker_env
        .entry("GOSUMDB".to_string())
        .or_insert_with(|| "sum.golang.org".to_string());
    docker_env
        .entry("CARGO_REGISTRIES_CRATES_IO_PROTOCOL".to_string())
        .or_insert_with(|| "sparse".to_string());
}

fn resolve_local_agent_egress_plan(
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_agent_config: &LocalAgentConfig,
) -> anyhow::Result<LocalAgentEgressPlan> {
    let request = runtime_spec.environment.network_resources.clone();

    let Some(request) = request else {
        if runtime_spec.permissions.can_access_network {
            anyhow::bail!("network access requires an explicit typed network_resources request");
        }
        return Ok(LocalAgentEgressPlan {
            network_none: true,
            tier: None,
        });
    };

    let requested_policy = policy_name_for_network_request(&request)?;
    let Some(policy_name) = requested_policy else {
        return Ok(LocalAgentEgressPlan {
            network_none: true,
            tier: None,
        });
    };

    let tree = local_agent_config
        .config_tree()
        .ok_or_else(|| anyhow::anyhow!("CONFIG_ROOT is required and config tree must be loaded"))?;
    let Some(policy) = tree.network_policies.get(policy_name) else {
        anyhow::bail!(
            "network policy '{}' is required by CONFIG_ROOT but was not loaded",
            policy_name
        );
    };

    let tier = if policy.unrestricted {
        Some(LocalAgentEgressTier::Full)
    } else if policy
        .docker_network
        .as_deref()
        .is_some_and(|network| network.eq_ignore_ascii_case("none"))
    {
        None
    } else {
        match policy_name {
            "llm-only" => Some(LocalAgentEgressTier::Llm),
            "github-only" => Some(LocalAgentEgressTier::Github),
            "build" => Some(LocalAgentEgressTier::Build),
            "none" => None,
            "full" => Some(LocalAgentEgressTier::Full),
            _ => anyhow::bail!(
                "network policy '{}' is unsupported by local-agent tier mapping",
                policy_name
            ),
        }
    };

    Ok(LocalAgentEgressPlan {
        network_none: tier.is_none(),
        tier,
    })
}

fn policy_name_for_network_request(
    request: &NetworkResourceRequest,
) -> anyhow::Result<Option<&'static str>> {
    match request {
        NetworkResourceRequest::None => Ok(None),
        NetworkResourceRequest::Full => Ok(Some("full")),
        NetworkResourceRequest::Resources(resources) => {
            let mut deduped = resources.clone();
            deduped.sort_by_key(|a| a.to_string());
            deduped.dedup();
            if deduped.is_empty() {
                return Ok(None);
            }
            if deduped
                .iter()
                .all(|resource| matches!(resource, NetworkResourceKind::Llm))
            {
                return Ok(Some("llm-only"));
            }
            if deduped == [NetworkResourceKind::Github] {
                return Ok(Some("github-only"));
            }
            if deduped.iter().all(|resource| {
                matches!(
                    resource,
                    NetworkResourceKind::Llm
                        | NetworkResourceKind::Github
                        | NetworkResourceKind::GoRegistry
                        | NetworkResourceKind::CratesRegistry
                )
            }) {
                return Ok(Some("build"));
            }
            anyhow::bail!(
                "requested network resources are unsupported by local-agent policies: {:?}",
                deduped
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise_process(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    supervise_process_with_mode(
        job,
        workspace_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        env,
        timeout_secs,
        events,
        cancel_rx,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn supervise_process_with_output_validation(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
    append: bool,
) -> anyhow::Result<ExecutionStatus> {
    let stdout_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stdout_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stdout_path)?
    };
    let stderr_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stderr_path)?
    };
    let transcript_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(transcript_jsonl_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(transcript_jsonl_path)?
    };

    let stdout_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stdout_std));
    let stderr_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stderr_std));
    let transcript_writer = Arc::new(Mutex::new(tokio::io::BufWriter::new(
        tokio::fs::File::from_std(transcript_std),
    )));

    let mut cmd = tokio::process::Command::new(&job.command);
    cmd.args(&job.args)
        .current_dir(workspace_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    eprintln!("info: spawning command: {} {:?}", job.command, job.args);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn command: {} {:?}", job.command, job.args))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stderr"))?;

    let stdout_events = events.cloned();
    let stdout_job_id = job.id;
    let stdout_transcript_writer = Arc::clone(&transcript_writer);
    let stdout_task = tokio::spawn(async move {
        pump_process_stream(
            stdout,
            stdout_file,
            stdout_transcript_writer,
            "stdout",
            tokio::io::stdout(),
            stdout_events,
            stdout_job_id,
            pb::JobEventKind::JobEventStdout,
        )
        .await
    });

    let stderr_events = events.cloned();
    let stderr_job_id = job.id;
    let stderr_transcript_writer = Arc::clone(&transcript_writer);
    let stderr_task = tokio::spawn(async move {
        pump_process_stream(
            stderr,
            stderr_file,
            stderr_transcript_writer,
            "stderr",
            tokio::io::stderr(),
            stderr_events,
            stderr_job_id,
            pb::JobEventKind::JobEventStderr,
        )
        .await
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();
    let timeout = timeout_secs.map(Duration::from_secs);

    let status = loop {
        tokio::select! {
            wait_result = child.wait() => {
                break wait_result?;
            }
            _ = ticker.tick() => {
                if cancel_requested(&cancel_rx) {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                "cancellation requested by server".to_string(),
                            )
                            .await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    cleanup_named_docker_container_best_effort(job).await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return Ok(ExecutionStatus::Cancelled);
                }

                if validate_opencode_output(
                    output_path,
                    valid_next_events,
                    work_item_context,
                    environment_table,
                )
                .is_ok()
                {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "output.json validated; stopping lingering process {}",
                                    job.id
                                ),
                            )
                            .await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return Ok(ExecutionStatus::Exited(0));
                }

                if let Some(timeout) = timeout
                    && start.elapsed() >= timeout {
                        if let Some(events) = events {
                            events
                                .emit(
                                    job.id,
                                    pb::JobEventKind::JobEventSystem,
                                    format!("timeout after {}s", timeout.as_secs()),
                                )
                                .await;
                        }
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        let _ = stdout_task.await;
                        let _ = stderr_task.await;
                        return Ok(ExecutionStatus::TimedOut);
                    }
            }
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(ExecutionStatus::Exited(status.code().unwrap_or(1)))
}

#[allow(clippy::too_many_arguments)]
async fn supervise_process_with_mode(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
    append: bool,
) -> anyhow::Result<ExecutionStatus> {
    let stdout_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stdout_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stdout_path)?
    };
    let stderr_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stderr_path)?
    };
    let transcript_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(transcript_jsonl_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(transcript_jsonl_path)?
    };

    let stdout_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stdout_std));
    let stderr_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stderr_std));
    let transcript_writer = Arc::new(Mutex::new(tokio::io::BufWriter::new(
        tokio::fs::File::from_std(transcript_std),
    )));

    let mut cmd = tokio::process::Command::new(&job.command);
    cmd.args(&job.args)
        .current_dir(workspace_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    eprintln!(
        "info: supervising process: cmd={} args_count={} cwd={} cwd_exists={}",
        job.command,
        job.args.len(),
        workspace_dir.display(),
        workspace_dir.exists()
    );
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn '{}' (args={}) in cwd={}",
            job.command,
            job.args.len(),
            workspace_dir.display()
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stderr"))?;

    let stdout_events = events.cloned();
    let stdout_job_id = job.id;
    let stdout_transcript_writer = Arc::clone(&transcript_writer);
    let stdout_task = tokio::spawn(async move {
        pump_process_stream(
            stdout,
            stdout_file,
            stdout_transcript_writer,
            "stdout",
            tokio::io::stdout(),
            stdout_events,
            stdout_job_id,
            pb::JobEventKind::JobEventStdout,
        )
        .await
    });

    let stderr_events = events.cloned();
    let stderr_job_id = job.id;
    let stderr_transcript_writer = Arc::clone(&transcript_writer);
    let stderr_task = tokio::spawn(async move {
        pump_process_stream(
            stderr,
            stderr_file,
            stderr_transcript_writer,
            "stderr",
            tokio::io::stderr(),
            stderr_events,
            stderr_job_id,
            pb::JobEventKind::JobEventStderr,
        )
        .await
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();

    let status = loop {
        tokio::select! {
            wait_result = child.wait() => {
                break wait_result?;
            }
            _ = ticker.tick() => {
                if cancel_requested(&cancel_rx) {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                "cancellation requested by server".to_string(),
                            )
                            .await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    cleanup_named_docker_container_best_effort(job).await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return Ok(ExecutionStatus::Cancelled);
                }

                if let Some(timeout_secs) = timeout_secs
                    && start.elapsed() >= Duration::from_secs(timeout_secs)
                {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!("timeout after {timeout_secs}s"),
                            )
                            .await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Ok(ExecutionStatus::TimedOut);
                }
            }
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(ExecutionStatus::Exited(status.code().unwrap_or(1)))
}

#[allow(clippy::too_many_arguments)]
async fn pump_process_stream<R>(
    reader: R,
    mut log_file: tokio::io::BufWriter<tokio::fs::File>,
    transcript_writer: Arc<Mutex<tokio::io::BufWriter<tokio::fs::File>>>,
    stream: &'static str,
    mut parent_out: impl AsyncWrite + Unpin,
    events: Option<JobEventSink>,
    job_id: uuid::Uuid,
    kind: pb::JobEventKind,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        log_file.write_all(line.as_bytes()).await?;
        log_file.write_all(b"\n").await?;
        log_file.flush().await?;

        parent_out.write_all(line.as_bytes()).await?;
        parent_out.write_all(b"\n").await?;
        parent_out.flush().await?;

        let record = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "stream": stream,
            "line": line,
        });
        let mut encoded = serde_json::to_vec(&record)?;
        encoded.push(b'\n');
        {
            let mut writer = transcript_writer.lock().await;
            writer.write_all(&encoded).await?;
            writer.flush().await?;
        }

        if let Some(events) = &events {
            let message = record
                .get("line")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            events.emit(job_id, kind, message).await;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_docker(
    job: &Job,
    local_agent_config: &LocalAgentConfig,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    let workspace_dir = resolve_to_absolute(workspace_dir)
        .with_context(|| format!("resolve workspace dir {}", workspace_dir.display()))?;
    let transcript_dir = resolve_to_absolute(Path::new(transcript_dir))
        .with_context(|| format!("resolve transcript dir {}", transcript_dir))?;

    let mut docker_env = env.clone();
    docker_env.insert("AGENT_WORKSPACE_DIR".to_string(), "/workspace".to_string());
    docker_env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        "/transcript".to_string(),
    );
    docker_env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        "/transcript/context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        "/transcript/.agent-context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        "/transcript/transcript.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        "/transcript/job-events.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        "/transcript/output.json".to_string(),
    );
    docker_env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        "/transcript/stdout.log".to_string(),
    );
    docker_env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        "/transcript/stderr.log".to_string(),
    );
    docker_env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        "/transcript/run.json".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        "/transcript/prompt/user.txt".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        "/transcript/prompt/system.txt".to_string(),
    );
    // Ensure HOME is writable for tools like opencode that create config dirs.
    docker_env
        .entry("HOME".to_string())
        .or_insert_with(|| "/home/agent".to_string());
    docker_env
        .entry("XDG_DATA_HOME".to_string())
        .or_insert_with(|| "/home/agent/.local/share".to_string());
    docker_env
        .entry("XDG_CONFIG_HOME".to_string())
        .or_insert_with(|| "/home/agent/.config".to_string());
    docker_env
        .entry("XDG_STATE_HOME".to_string())
        .or_insert_with(|| "/home/agent/.local/state".to_string());
    if job.repo.is_some() {
        append_git_config_env(&mut docker_env, "safe.directory", "/workspace");
        append_git_identity_env_defaults(&mut docker_env);
    }

    let mut args = vec!["run".to_string(), "--rm".to_string()];

    if let Some(user) = docker_run_user_arg(&workspace_dir) {
        args.push("--user".to_string());
        args.push(user);
    }

    args.push("--security-opt=no-new-privileges".to_string());

    let memory_limit = runtime_spec
        .environment
        .memory_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_MEMORY_LIMIT);
    let cpu_limit = runtime_spec
        .environment
        .cpu_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_CPU_LIMIT);
    args.push(format!("--memory={memory_limit}"));
    args.push(format!("--cpus={cpu_limit}"));

    let mut docker_network_none = local_egress_plan.network_none;
    let restricted_tier = match local_egress_plan.tier {
        Some(LocalAgentEgressTier::Llm)
        | Some(LocalAgentEgressTier::Github)
        | Some(LocalAgentEgressTier::Build) => local_egress_plan.tier,
        _ => None,
    };
    if !runtime_spec.permissions.can_access_network && !docker_network_none {
        let warning =
            "WARNING: forcing --network=none because can_access_network is false in permissions";
        eprintln!("{warning}");
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    warning.to_string(),
                )
                .await;
        }
        docker_network_none = true;
    }
    if docker_network_none {
        args.push("--network=none".to_string());
    } else if let Some(tier) = restricted_tier {
        for mount in &runtime_spec.environment.file_mounts {
            if is_restricted_breakout_mount(mount) {
                anyhow::bail!(
                    "restricted egress tier '{}' rejects breakout mount '{}:{}'",
                    tier.as_str(),
                    mount.host_path,
                    mount.container_path
                );
            }
        }

        let llm_provider = llm_provider_from_env(env);
        let restricted_runtime = egress_manager
            .prepare_restricted_tier(tier, llm_provider.as_deref())
            .await
            .with_context(|| {
                format!(
                    "failed preparing restricted egress tier '{}' for job {}",
                    tier.as_str(),
                    job.id
                )
            })?;
        restricted_runtime.apply_container_network_controls(&mut args, &mut docker_env);

        if matches!(tier, LocalAgentEgressTier::Build) {
            apply_build_egress_env_defaults(&mut docker_env, local_agent_config);
        }
    }
    if let Some(tier) = local_egress_plan.tier
        && let Some(events) = events
    {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "local-agent egress tier resolved to '{}': MVP policy",
                    tier.as_str()
                ),
            )
            .await;
    }

    let workspace_mount_mode = if runtime_spec.permissions.can_write_workspace {
        "rw"
    } else {
        "ro"
    };
    if !runtime_spec.permissions.can_write_workspace {
        args.push("--read-only".to_string());
        args.push("--tmpfs".to_string());
        args.push("/tmp:rw,size=1g".to_string());
    }

    args.extend([
        "-v".to_string(),
        format!(
            "{}:/workspace:{workspace_mount_mode}",
            workspace_dir.to_string_lossy()
        ),
        "-v".to_string(),
        format!("{}:/transcript", transcript_dir.to_string_lossy()),
        "-w".to_string(),
        "/workspace".to_string(),
    ]);

    if let Some(mirror_dir) = primary_repo_mirror_dir {
        let mirror_git_dir = resolve_to_absolute(mirror_dir)
            .with_context(|| format!("resolve primary repo mirror {}", mirror_dir.display()))?
            .join(".git");
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:{workspace_mount_mode}",
            mirror_git_dir.to_string_lossy(),
            mirror_git_dir.to_string_lossy()
        ));
    }

    for mount in additional_repo_mounts {
        let host_path = resolve_to_absolute(&mount.host_path).with_context(|| {
            format!(
                "resolve additional repo mount {}",
                mount.host_path.display()
            )
        })?;
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:ro",
            host_path.to_string_lossy(),
            mount.container_path
        ));
    }

    let work_item_context = work_item_contract_from_job_context(&job.context);
    let allow_remote_credentials = job_allows_remote_credentials(&work_item_context);
    let required_credential_providers = authorized_credential_provider_names(job)?;
    if required_credential_providers
        .iter()
        .any(|provider| provider.starts_with("github:"))
    {
        anyhow::bail!("GitHub credentials are restricted to managed GitHub actions");
    }
    if !allow_remote_credentials && !required_credential_providers.is_empty() {
        anyhow::bail!("job requires credential providers outside its remote-access envelope");
    }
    let resolved_credentials = if allow_remote_credentials {
        resolve_configured_credentials(local_agent_config, &required_credential_providers).await?
    } else {
        ResolvedCredentials::default()
    };
    validate_general_job_credentials(&resolved_credentials)?;
    append_resolved_credentials_to_docker_invocation(
        &resolved_credentials,
        &mut args,
        &mut docker_env,
    );

    for mount in &runtime_spec.environment.file_mounts {
        let mut spec = format!("{}:{}", mount.host_path, mount.container_path);
        if mount.read_only {
            spec.push_str(":ro");
        }
        args.push("-v".to_string());
        args.push(spec);
    }
    let mcp_bridges = configure_mcp_bridges_for_docker(
        &mut args,
        &mut docker_env,
        local_agent_config,
        job,
        events,
    )
    .await?;
    for (k, v) in &docker_env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }
    let container_image = runtime_container_image.ok_or_else(|| {
        anyhow::anyhow!("runtime container_image must be set via CONFIG_ROOT configuration")
    })?;
    args.push("--entrypoint".to_string());
    args.push(job.command.clone());
    args.push(container_image);
    args.extend(job.args.clone());

    let docker_job = Job {
        args,
        command: "docker".to_string(),
        ..job.clone()
    };
    eprintln!(
        "info: docker invocation: {} {}",
        docker_job.command,
        docker_job.args.join(" ")
    );
    let run_result = supervise_process(
        &docker_job,
        &workspace_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        &HashMap::new(),
        timeout_secs,
        events,
        cancel_rx,
    )
    .await?;
    for bridge in &mcp_bridges {
        bridge.stop();
    }
    cleanup_credential_temp_files_best_effort(&resolved_credentials.temp_files);
    Ok(run_result)
}

struct DockerRuntimeInvocation {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    timeout_secs: Option<u64>,
    workspace_dir: PathBuf,
    transcript_jsonl_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    credential_temp_files: Vec<PathBuf>,
    mcp_bridges: Vec<McpBridgeInstance>,
}

#[allow(clippy::too_many_arguments)]
async fn run_docker_opencode_with_validation_retries(
    job: &Job,
    local_agent_config: &LocalAgentConfig,
    opencode: &OpencodeManagedSpec,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    output_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    valid_next_events: Vec<String>,
    work_item_context: serde_json::Value,
    environment_table: &EnvironmentValidationTable,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    let invocation = build_docker_runtime_invocation(
        job,
        local_agent_config,
        workspace_dir,
        transcript_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        env,
        timeout_secs,
        runtime_container_image,
        runtime_spec,
        local_egress_plan,
        egress_manager,
        primary_repo_mirror_dir,
        additional_repo_mounts,
        events,
    )
    .await?;

    run_opencode_validation_loop(
        job,
        opencode,
        &invocation,
        output_path,
        &valid_next_events,
        &work_item_context,
        environment_table,
        events,
        cancel_rx,
    )
    .await
    .inspect(|_| {
        for bridge in &invocation.mcp_bridges {
            bridge.stop();
        }
        cleanup_credential_temp_files_best_effort(&invocation.credential_temp_files);
    })
    .inspect_err(|_| {
        for bridge in &invocation.mcp_bridges {
            bridge.stop();
        }
        cleanup_credential_temp_files_best_effort(&invocation.credential_temp_files);
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_opencode_validation_loop(
    job: &Job,
    opencode: &OpencodeManagedSpec,
    invocation: &DockerRuntimeInvocation,
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    let max_attempts = opencode.max_validation_attempts.max(1);
    let _ = std::fs::remove_file(output_path);

    let container_name = format!("agent-hub-opencode-{}", job.id.simple());
    let mut attempt: u32 = 1;
    let mut continuation_prompt: Option<String> = None;
    let shell_prelude = opencode_shell_prelude(job);

    spawn_opencode_runtime_container(invocation, &container_name).await?;

    loop {
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    format!(
                        "opencode validation attempt {attempt}/{max_attempts} ({})",
                        if continuation_prompt.is_none() {
                            "run"
                        } else {
                            "run --continue"
                        }
                    ),
                )
                .await;
        }

        let status = run_docker_opencode_attempt(
            job,
            invocation,
            &container_name,
            continuation_prompt.as_deref(),
            &shell_prelude,
            attempt > 1,
            output_path,
            valid_next_events,
            work_item_context,
            environment_table,
            events,
            cancel_rx.clone(),
        )
        .await?;

        if matches!(status, ExecutionStatus::Cancelled) {
            if let Err(error) = cleanup_opencode_container(&container_name).await {
                eprintln!(
                    "warning: failed to cleanup opencode container {}: {error:#}",
                    container_name
                );
            }
            return Ok(status);
        }

        if !matches!(status, ExecutionStatus::Exited(0)) {
            if let Err(error) = cleanup_opencode_container(&container_name).await {
                eprintln!(
                    "warning: failed to cleanup opencode container {}: {error:#}",
                    container_name
                );
            }
            return Ok(status);
        }

        match validate_opencode_output(
            output_path,
            valid_next_events,
            work_item_context,
            environment_table,
        ) {
            Ok(()) => {
                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!("opencode output validation passed on attempt {attempt}"),
                        )
                        .await;
                }
                if let Err(error) = cleanup_opencode_container(&container_name).await {
                    eprintln!(
                        "warning: failed to cleanup opencode container {}: {error:#}",
                        container_name
                    );
                }
                return Ok(status);
            }
            Err(validation_error) => {
                if attempt >= max_attempts {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "opencode output validation failed after {attempt} attempts: {validation_error}"
                                ),
                            )
                            .await;
                    }
                    if let Err(error) = cleanup_opencode_container(&container_name).await {
                        eprintln!(
                            "warning: failed to cleanup opencode container {}: {error:#}",
                            container_name
                        );
                    }
                    anyhow::bail!(
                        "opencode validation failed after {attempt} attempts: {validation_error}"
                    );
                }

                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!(
                                "opencode output validation failed (attempt {attempt}/{max_attempts}): {validation_error}; retrying with run --continue"
                            ),
                        )
                        .await;
                }
                continuation_prompt = Some(build_opencode_correction_prompt(
                    valid_next_events,
                    &validation_error.to_string(),
                    environment_table,
                ));
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_docker_opencode_attempt(
    job: &Job,
    invocation: &DockerRuntimeInvocation,
    container_name: &str,
    continuation_prompt: Option<&str>,
    shell_prelude: &str,
    append_logs: bool,
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    let opencode_cmd = if let Some(prompt) = continuation_prompt {
        build_opencode_continue_script(shell_prelude, prompt)
    } else {
        String::new()
    };

    let docker_job = Job {
        command: invocation.command.clone(),
        args: if continuation_prompt.is_some() {
            vec![
                "exec".to_string(),
                container_name.to_string(),
                "sh".to_string(),
                "-lc".to_string(),
                opencode_cmd,
            ]
        } else {
            let mut exec_args = vec!["exec".to_string(), container_name.to_string()];
            exec_args.push(job.command.clone());
            exec_args.extend(job.args.clone());
            exec_args
        },
        ..job.clone()
    };

    if append_logs {
        supervise_process_with_output_validation(
            &docker_job,
            &invocation.workspace_dir,
            &invocation.transcript_jsonl_path,
            &invocation.stdout_path,
            &invocation.stderr_path,
            &invocation.env,
            invocation.timeout_secs,
            output_path,
            valid_next_events,
            work_item_context,
            environment_table,
            events,
            cancel_rx,
            true,
        )
        .await
    } else {
        supervise_process_with_output_validation(
            &docker_job,
            &invocation.workspace_dir,
            &invocation.transcript_jsonl_path,
            &invocation.stdout_path,
            &invocation.stderr_path,
            &invocation.env,
            invocation.timeout_secs,
            output_path,
            valid_next_events,
            work_item_context,
            environment_table,
            events,
            cancel_rx,
            false,
        )
        .await
    }
}

fn cancel_requested(cancel_rx: &Option<watch::Receiver<bool>>) -> bool {
    cancel_rx.as_ref().is_some_and(|rx| *rx.borrow())
}

async fn cleanup_named_docker_container_best_effort(job: &Job) {
    if job.command != "docker" {
        return;
    }
    let Some(name_index) = job.args.iter().position(|arg| arg == "--name") else {
        return;
    };
    let Some(name) = job.args.get(name_index + 1) else {
        return;
    };
    let _ = cleanup_opencode_container(name).await;
}

async fn spawn_opencode_runtime_container(
    invocation: &DockerRuntimeInvocation,
    container_name: &str,
) -> anyhow::Result<()> {
    let _ = tokio::process::Command::new("docker")
        .arg("rm")
        .arg("-f")
        .arg(container_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    let run_args = build_opencode_spawn_run_args(&invocation.args, container_name)?;

    let mut cmd = tokio::process::Command::new(&invocation.command);
    cmd.args(&run_args).current_dir(&invocation.workspace_dir);
    for (k, v) in &invocation.env {
        cmd.env(k, v);
    }

    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "failed to start opencode runtime container {}: {}",
            container_name,
            status
        )
    }
}

fn build_opencode_spawn_run_args(
    invocation_args: &[String],
    container_name: &str,
) -> anyhow::Result<Vec<String>> {
    let entry_idx = invocation_args
        .iter()
        .position(|arg| arg == "--entrypoint")
        .context("docker invocation missing --entrypoint")?;
    let image_idx = entry_idx.saturating_add(2);
    if image_idx >= invocation_args.len() {
        anyhow::bail!("docker invocation missing image argument");
    }

    let mut run_args = invocation_args[..entry_idx].to_vec();
    run_args.retain(|arg| arg != "--rm");
    run_args.push("--name".to_string());
    run_args.push(container_name.to_string());
    run_args.push("-d".to_string());
    run_args.push("--entrypoint".to_string());
    run_args.push("sh".to_string());
    run_args.push(invocation_args[image_idx].clone());
    run_args.push("-lc".to_string());
    run_args.push("while true; do sleep 600; done".to_string());
    Ok(run_args)
}

fn opencode_shell_prelude(job: &Job) -> String {
    if job.command != "sh" {
        return String::new();
    }
    if job.args.first().map(String::as_str) != Some("-lc") {
        return String::new();
    }
    let Some(script) = job.args.get(1) else {
        return String::new();
    };
    let marker = "if [ -f \"$AGENT_PROMPT_USER_PATH\" ]";
    let Some((prefix, _)) = script.split_once(marker) else {
        return String::new();
    };
    prefix.trim().trim_end_matches(';').trim().to_string()
}

fn build_opencode_continue_script(shell_prelude: &str, prompt: &str) -> String {
    let mut script = String::new();
    if !shell_prelude.is_empty() {
        script.push_str(shell_prelude);
        script.push_str("; ");
    }
    script.push_str("cat <<'AGENT_HUB_RETRY_PROMPT' >/tmp/agent-hub-retry-prompt.txt\n");
    script.push_str(prompt);
    script.push_str(
        "\nAGENT_HUB_RETRY_PROMPT\nopencode run --continue < /tmp/agent-hub-retry-prompt.txt",
    );
    script
}

fn validate_opencode_output(
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
) -> anyhow::Result<()> {
    if !output_path.exists() {
        anyhow::bail!("output.json missing")
    }

    let bytes = std::fs::read(output_path).context("read output.json")?;
    let raw_output: serde_json::Value =
        serde_json::from_slice(&bytes).context("output.json parse failed")?;
    let typed_output: AgentOutput = serde_json::from_value(raw_output.clone())
        .context("output.json is not valid AgentOutput")?;

    validate_agent_output_next_event(&typed_output, valid_next_events)
        .map_err(anyhow::Error::msg)?;

    validate_environment_selection_structure(&raw_output, work_item_context, environment_table)?;
    validate_triage_repo_rules(work_item_context, &typed_output)?;
    Ok(())
}

/// Returns `true` when the executing job is the `__triage__` pseudo-workflow's
/// triage step. Triage is the universal ingress route into any selectable
/// workflow, so its output is held to a stricter schema (must declare an
/// `environment` and a concrete `repo` on success).
fn is_triage_job(work_item_context: &serde_json::Value) -> bool {
    let workflow_id = work_item_context
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    workflow_id == "__triage__"
}

fn job_allows_remote_credentials(work_item_context: &serde_json::Value) -> bool {
    !is_triage_job(work_item_context)
}

fn validate_triage_mount_and_credential_requests(
    triage_job: bool,
    runner: &ResolvedRunnerSpec,
    runtime: &protocol::CanonicalRuntimeSpec,
    config: &LocalAgentConfig,
) -> anyhow::Result<()> {
    if !triage_job {
        return Ok(());
    }
    if !runner.file_mounts.is_empty() || !runner.requires_credentials.is_empty() {
        anyhow::bail!(
            "triage runner must not declare arbitrary file mounts or credential providers"
        );
    }
    let approved_roots = canonical_approved_repository_roots(config)?;
    for mount in &runtime.environment.file_mounts {
        if !mount.read_only || !mount.container_path.starts_with("/repos/") {
            anyhow::bail!("triage repository mounts must be read-only under /repos");
        }
        let source = std::fs::canonicalize(&mount.host_path)
            .with_context(|| format!("canonicalize triage repository mount {}", mount.host_path))?;
        if !approved_roots.iter().any(|root| source.starts_with(root)) {
            anyhow::bail!(
                "triage repository mount '{}' is outside approved repository roots",
                source.display()
            );
        }
        if !source.join(".git").exists() || path_contains_credential_directory(&source) {
            anyhow::bail!(
                "triage mount '{}' is not an approved repository",
                source.display()
            );
        }
    }
    if !runtime.repos.is_empty() {
        anyhow::bail!("triage runtime must not request arbitrary repository mounts");
    }
    Ok(())
}

fn validate_environment_selection_structure(
    raw_output: &serde_json::Value,
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
) -> anyhow::Result<()> {
    let Some(environment) = raw_output.get("environment") else {
        if is_triage_job(work_item_context) {
            anyhow::bail!(
                "environment is required for triage success. Valid base environments: {}",
                environment_table.options_string()
            );
        }
        return Ok(());
    };

    let environment_obj = environment
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("environment must be an object"))?;

    let base = environment_obj
        .get("base")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("environment.base must be a string"))?;
    if base.is_empty() {
        anyhow::bail!("environment.base must be a non-empty string");
    }

    let overlays = environment_obj.get("overlays").ok_or_else(|| {
        anyhow::anyhow!("environment.overlays is required when environment is provided")
    })?;
    let overlays = overlays
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("environment.overlays must be an array of strings"))?;
    if overlays.iter().any(|overlay| !overlay.is_string()) {
        anyhow::bail!("environment.overlays must be an array of strings");
    }

    let Some(allowed_overlays) = environment_table.overlays_for_base(base) else {
        anyhow::bail!(
            "environment.base '{}' is invalid. Valid base environments: {}",
            base,
            environment_table.options_string()
        );
    };

    for overlay in overlays.iter().filter_map(serde_json::Value::as_str) {
        if !allowed_overlays.iter().any(|allowed| allowed == overlay) {
            anyhow::bail!(
                "environment overlay '{}' is invalid for base '{}'. Allowed overlays: {}",
                overlay,
                base,
                allowed_overlays.join(", ")
            );
        }
    }

    Ok(())
}

fn validate_triage_repo_rules(
    work_item_context: &serde_json::Value,
    output: &AgentOutput,
) -> anyhow::Result<()> {
    if !is_triage_job(work_item_context) {
        return Ok(());
    }

    let next_event = output.next_event.as_deref().unwrap_or_default();
    if next_event != "job_succeeded" {
        return Ok(());
    }

    let repo = output
        .repo
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("repo is required for triage success"))?;

    let repo_url = repo.repo_url.trim();
    let base_ref = repo.base_ref.trim();
    if repo_url.is_empty() || repo_url.contains("placeholder") || repo_url.contains("example") {
        anyhow::bail!("repo.repo_url must be a concrete repository URL for triage success");
    }
    if base_ref.is_empty() || base_ref.contains("placeholder") {
        anyhow::bail!("repo.base_ref must be concrete for triage success");
    }

    let candidate_repo_urls = work_item_context
        .get("repo_catalog")
        .and_then(serde_json::Value::as_array)
        .map(|catalog| {
            catalog
                .iter()
                .filter_map(serde_json::Value::as_object)
                .filter_map(|entry| entry.get("repo_url"))
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !candidate_repo_urls.is_empty() && !candidate_repo_urls.iter().any(|url| url == repo_url) {
        anyhow::bail!(
            "repo.repo_url '{}' must match one of work_item.repo_catalog candidates",
            repo_url
        );
    }

    Ok(())
}

fn build_opencode_correction_prompt(
    valid_next_events: &[String],
    validation_error: &str,
    environment_table: &EnvironmentValidationTable,
) -> String {
    format!(
        "Your previous output.json failed validation in local-agent.\n\nValidation error:\n{validation_error}\n\nYou must write a corrected /transcript/output.json that:\n1) parses as AgentOutput JSON\n2) sets next_event to one of: {:?}\n3) if next_event is job_succeeded for a __triage__ job, include a concrete repo object with non-empty repo_url and base_ref\n4) if environment is present, it must be an object with non-empty string base and overlays as an array of strings, and semantic values must be valid.\n5) valid base environments: {}.\n\nOnly fix output.json and avoid unrelated changes.",
        valid_next_events,
        environment_table.options_string(),
    )
}

async fn cleanup_opencode_container(container_name: &str) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("rm").arg("-f").arg(container_name);
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("docker rm -f {} failed with {}", container_name, status)
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_docker_runtime_invocation(
    job: &Job,
    local_agent_config: &LocalAgentConfig,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    events: Option<&JobEventSink>,
) -> anyhow::Result<DockerRuntimeInvocation> {
    let workspace_dir = resolve_to_absolute(workspace_dir)
        .with_context(|| format!("resolve workspace dir {}", workspace_dir.display()))?;
    let transcript_dir = resolve_to_absolute(Path::new(transcript_dir))
        .with_context(|| format!("resolve transcript dir {}", transcript_dir))?;

    let mut docker_env = env.clone();
    docker_env.insert("AGENT_WORKSPACE_DIR".to_string(), "/workspace".to_string());
    docker_env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        "/transcript".to_string(),
    );
    docker_env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        "/transcript/context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        "/transcript/.agent-context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        "/transcript/transcript.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        "/transcript/job-events.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        "/transcript/output.json".to_string(),
    );
    docker_env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        "/transcript/stdout.log".to_string(),
    );
    docker_env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        "/transcript/stderr.log".to_string(),
    );
    docker_env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        "/transcript/run.json".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        "/transcript/prompt/user.txt".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        "/transcript/prompt/system.txt".to_string(),
    );
    // Ensure HOME is writable for tools like opencode that create config dirs.
    docker_env
        .entry("HOME".to_string())
        .or_insert_with(|| "/home/agent".to_string());
    docker_env
        .entry("XDG_DATA_HOME".to_string())
        .or_insert_with(|| "/home/agent/.local/share".to_string());
    docker_env
        .entry("XDG_CONFIG_HOME".to_string())
        .or_insert_with(|| "/home/agent/.config".to_string());
    docker_env
        .entry("XDG_STATE_HOME".to_string())
        .or_insert_with(|| "/home/agent/.local/state".to_string());
    if job.repo.is_some() {
        append_git_config_env(&mut docker_env, "safe.directory", "/workspace");
        append_git_identity_env_defaults(&mut docker_env);
    }

    let mut args = vec!["run".to_string(), "--rm".to_string()];
    let container_image = runtime_container_image.ok_or_else(|| {
        anyhow::anyhow!("runtime container_image must be set via CONFIG_ROOT configuration")
    })?;

    if let Some(user) = docker_run_user_arg(&workspace_dir) {
        args.push("--user".to_string());
        args.push(user);
    }

    args.push("--security-opt=no-new-privileges".to_string());

    let memory_limit = runtime_spec
        .environment
        .memory_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_MEMORY_LIMIT);
    let cpu_limit = runtime_spec
        .environment
        .cpu_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_CPU_LIMIT);
    args.push(format!("--memory={memory_limit}"));
    args.push(format!("--cpus={cpu_limit}"));

    let mut docker_network_none = local_egress_plan.network_none;
    let restricted_tier = match local_egress_plan.tier {
        Some(LocalAgentEgressTier::Llm)
        | Some(LocalAgentEgressTier::Github)
        | Some(LocalAgentEgressTier::Build) => local_egress_plan.tier,
        _ => None,
    };
    if !runtime_spec.permissions.can_access_network && !docker_network_none {
        let warning =
            "WARNING: forcing --network=none because can_access_network is false in permissions";
        eprintln!("{warning}");
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    warning.to_string(),
                )
                .await;
        }
        docker_network_none = true;
    }
    if docker_network_none {
        args.push("--network=none".to_string());
    } else if let Some(tier) = restricted_tier {
        for mount in &runtime_spec.environment.file_mounts {
            if is_restricted_breakout_mount(mount) {
                anyhow::bail!(
                    "restricted egress tier '{}' rejects breakout mount '{}:{}'",
                    tier.as_str(),
                    mount.host_path,
                    mount.container_path
                );
            }
        }

        let llm_provider = llm_provider_from_env(env);
        let restricted_runtime = egress_manager
            .prepare_restricted_tier(tier, llm_provider.as_deref())
            .await
            .with_context(|| {
                format!(
                    "failed preparing restricted egress tier '{}' for job {}",
                    tier.as_str(),
                    job.id
                )
            })?;
        restricted_runtime.apply_container_network_controls(&mut args, &mut docker_env);

        if matches!(tier, LocalAgentEgressTier::Build) {
            apply_build_egress_env_defaults(&mut docker_env, local_agent_config);
        }
    }
    if let Some(tier) = local_egress_plan.tier
        && let Some(events) = events
    {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "local-agent egress tier resolved to '{}': MVP policy",
                    tier.as_str()
                ),
            )
            .await;
    }

    let workspace_mount_mode = if runtime_spec.permissions.can_write_workspace {
        "rw"
    } else {
        "ro"
    };
    if !runtime_spec.permissions.can_write_workspace {
        args.push("--read-only".to_string());
        args.push("--tmpfs".to_string());
        args.push("/tmp:rw,size=1g".to_string());
    }

    args.extend([
        "-v".to_string(),
        format!(
            "{}:/workspace:{workspace_mount_mode}",
            workspace_dir.to_string_lossy()
        ),
        "-v".to_string(),
        format!("{}:/transcript", transcript_dir.to_string_lossy()),
        "-w".to_string(),
        "/workspace".to_string(),
    ]);

    if let Some(mirror_dir) = primary_repo_mirror_dir {
        let mirror_git_dir = resolve_to_absolute(mirror_dir)
            .with_context(|| format!("resolve primary repo mirror {}", mirror_dir.display()))?
            .join(".git");
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:{workspace_mount_mode}",
            mirror_git_dir.to_string_lossy(),
            mirror_git_dir.to_string_lossy()
        ));
    }

    for mount in additional_repo_mounts {
        let host_path = resolve_to_absolute(&mount.host_path).with_context(|| {
            format!(
                "resolve additional repo mount {}",
                mount.host_path.display()
            )
        })?;
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:ro",
            host_path.to_string_lossy(),
            mount.container_path
        ));
    }

    let work_item_context = work_item_contract_from_job_context(&job.context);
    let allow_remote_credentials = job_allows_remote_credentials(&work_item_context);
    let required_credential_providers = authorized_credential_provider_names(job)?;
    if required_credential_providers
        .iter()
        .any(|provider| provider.starts_with("github:"))
    {
        anyhow::bail!("GitHub credentials are restricted to managed GitHub actions");
    }
    let resolved_credentials = if allow_remote_credentials {
        resolve_configured_credentials(local_agent_config, &required_credential_providers).await?
    } else {
        ResolvedCredentials::default()
    };
    validate_general_job_credentials(&resolved_credentials)?;
    append_resolved_credentials_to_docker_invocation(
        &resolved_credentials,
        &mut args,
        &mut docker_env,
    );

    for mount in &runtime_spec.environment.file_mounts {
        let mut spec = format!("{}:{}", mount.host_path, mount.container_path);
        if mount.read_only {
            spec.push_str(":ro");
        }
        args.push("-v".to_string());
        args.push(spec);
    }
    let mcp_bridges = configure_mcp_bridges_for_docker(
        &mut args,
        &mut docker_env,
        local_agent_config,
        job,
        events,
    )
    .await?;
    for (k, v) in &docker_env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push("--entrypoint".to_string());
    args.push(job.command.clone());
    args.push(container_image);
    args.extend(job.args.clone());

    Ok(DockerRuntimeInvocation {
        command: "docker".to_string(),
        args,
        env: HashMap::new(),
        timeout_secs,
        workspace_dir,
        transcript_jsonl_path: transcript_jsonl_path.to_path_buf(),
        stdout_path: stdout_path.to_path_buf(),
        stderr_path: stderr_path.to_path_buf(),
        credential_temp_files: resolved_credentials.temp_files,
        mcp_bridges,
    })
}

async fn configure_mcp_bridges_for_docker(
    args: &mut Vec<String>,
    docker_env: &mut HashMap<String, String>,
    local_agent_config: &LocalAgentConfig,
    job: &Job,
    events: Option<&JobEventSink>,
) -> anyhow::Result<Vec<McpBridgeInstance>> {
    let work_item_context = work_item_contract_from_job_context(&job.context);
    if is_triage_job(&work_item_context) {
        return Ok(Vec::new());
    }
    let mut bridges = Vec::new();

    for bridge_config in &local_agent_config.mcp_bridges {
        match McpBridgeInstance::start(bridge_config).await {
            Ok(bridge) => {
                args.push("-v".to_string());
                args.push(bridge.docker_mount_arg());

                let container_path = container_socket_path(bridge_config);
                if let Some(env_var) = mcp_socket_env_var(&bridge.name) {
                    docker_env.insert(env_var, container_path);
                } else {
                    let warning = format!(
                        "warning: MCP bridge '{}' has invalid env var name; skipping MCP_*_SOCKET export",
                        bridge.name
                    );
                    eprintln!("{warning}");
                    if let Some(events) = events {
                        events
                            .emit(job.id, pb::JobEventKind::JobEventSystem, warning)
                            .await;
                    }
                }

                bridges.push(bridge);
            }
            Err(error) => {
                let _ = events;
                return Err(error).with_context(|| {
                    format!(
                        "failed to start required MCP bridge '{}'",
                        bridge_config.name
                    )
                });
            }
        }
    }

    Ok(bridges)
}

async fn resolve_configured_credentials(
    local_agent_config: &LocalAgentConfig,
    required_provider_names: &[String],
) -> anyhow::Result<ResolvedCredentials> {
    if required_provider_names.is_empty() {
        return Ok(ResolvedCredentials::default());
    }
    let providers = required_provider_names
        .iter()
        .map(|name| {
            local_agent_config
                .credential_providers
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("required credential provider '{name}' is unavailable")
                })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    resolve_all(&providers).await
}

fn authorized_credential_provider_names(job: &Job) -> anyhow::Result<Vec<String>> {
    let request = protocol::authorization_request_from_job_context(&job.context)
        .context("job has no resolved authorization request")?;
    let mut providers = request
        .permission_envelope
        .github_capabilities
        .into_iter()
        .map(|capability| capability.credential_provider)
        .collect::<Vec<_>>();
    if request.behavior.execution_kind != "command" {
        let profile: agent_hub_config::ResolvedRunnerProfile =
            serde_json::from_value(request.behavior.runner_profile)
                .context("authorization has invalid runner profile")?;
        providers.extend(
            profile
                .credential_providers
                .into_iter()
                .map(|provider| provider.name),
        );
    }
    providers.sort();
    providers.dedup();
    Ok(providers)
}

fn append_resolved_credentials_to_docker_invocation(
    resolved: &ResolvedCredentials,
    args: &mut Vec<String>,
    docker_env: &mut HashMap<String, String>,
) {
    for (host_path, container_path, read_only) in &resolved.file_mounts {
        let mut spec = format!("{host_path}:{container_path}");
        if *read_only {
            spec.push_str(":ro");
        }
        args.push("-v".to_string());
        args.push(spec);
    }

    for (key, value) in &resolved.env_vars {
        docker_env
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
}

fn validate_general_job_credentials(resolved: &ResolvedCredentials) -> anyhow::Result<()> {
    const MANAGED_GITHUB_ENV: [&str; 6] = [
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GH_HOST",
        "GH_CONFIG_DIR",
        "AGENT_HUB_GITHUB_TOKEN",
    ];
    if MANAGED_GITHUB_ENV
        .iter()
        .any(|name| resolved.env_vars.contains_key(*name))
    {
        cleanup_credential_temp_files(&resolved.temp_files)?;
        anyhow::bail!("GitHub credentials are restricted to managed GitHub actions");
    }
    Ok(())
}

fn cleanup_credential_temp_files_best_effort(temp_files: &[PathBuf]) {
    for path in temp_files {
        if let Err(error) = std::fs::remove_file(path) {
            eprintln!(
                "warning: failed to remove credential temp file {}: {error:#}",
                path.display()
            );
        }
    }
}

fn append_git_config_env(env: &mut HashMap<String, String>, key: &str, value: &str) {
    let count = env
        .get("GIT_CONFIG_COUNT")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0);
    env.insert(format!("GIT_CONFIG_KEY_{count}"), key.to_string());
    env.insert(format!("GIT_CONFIG_VALUE_{count}"), value.to_string());
    env.insert("GIT_CONFIG_COUNT".to_string(), (count + 1).to_string());
}

fn docker_run_user_arg(workspace_dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(workspace_dir).ok()?;
        Some(format!("{}:{}", metadata.uid(), metadata.gid()))
    }

    #[cfg(not(unix))]
    {
        let _ = workspace_dir;
        None
    }
}

fn append_git_identity_env_defaults(env: &mut HashMap<String, String>) {
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Agent Hub"),
        ("GIT_AUTHOR_EMAIL", "agent-hub@example.com"),
        ("GIT_COMMITTER_NAME", "Agent Hub"),
        ("GIT_COMMITTER_EMAIL", "agent-hub@example.com"),
    ] {
        env.entry(key.to_string())
            .or_insert_with(|| value.to_string());
    }
}

fn is_restricted_breakout_mount(mount: &protocol::orchestration::PlannedFileMount) -> bool {
    let host = normalize_mount_path_for_policy(&mount.host_path);
    let container = normalize_mount_path_for_policy(&mount.container_path);

    let dangerous_exact = [
        "/",
        "/proc",
        "/sys",
        "/dev",
        "/run/netns",
        "/var/run/docker.sock",
        "/run/docker.sock",
    ];
    if dangerous_exact
        .iter()
        .any(|path| host == *path || container == *path)
    {
        return true;
    }

    let dangerous_prefixes = [
        "/proc/",
        "/sys/",
        "/dev/",
        "/run/netns/",
        "/var/lib/docker/",
        "/var/run/docker.sock/",
        "/run/docker.sock/",
    ];
    if dangerous_prefixes
        .iter()
        .any(|prefix| host.starts_with(prefix) || container.starts_with(prefix))
    {
        return true;
    }

    host.contains("containerd.sock")
        || container.contains("containerd.sock")
        || host.contains("crio.sock")
        || container.contains("crio.sock")
}

fn normalize_mount_path_for_policy(path: &str) -> String {
    let normalized = path.trim().to_ascii_lowercase();
    if normalized == "/" {
        return normalized;
    }
    normalized.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod restricted_mount_tests {
    use super::{
        append_git_config_env, append_git_identity_env_defaults, docker_run_user_arg,
        is_restricted_breakout_mount,
    };
    use std::collections::HashMap;

    #[test]
    fn restricted_mount_rejects_obvious_breakout_paths() {
        let dangerous = [
            ("/var/run/docker.sock", "/var/run/docker.sock"),
            ("/", "/host"),
            ("/proc", "/host-proc"),
            ("/sys", "/host-sys"),
            ("/dev", "/host-dev"),
            ("/run/netns", "/netns"),
        ];

        for (host, container) in dangerous {
            let mount = protocol::orchestration::PlannedFileMount {
                host_path: host.to_string(),
                container_path: container.to_string(),
                read_only: true,
            };
            assert!(
                is_restricted_breakout_mount(&mount),
                "{} -> {}",
                host,
                container
            );
        }
    }

    #[test]
    fn restricted_mount_allows_normal_project_mounts() {
        let mount = protocol::orchestration::PlannedFileMount {
            host_path: "/home/user/project/.cargo".to_string(),
            container_path: "/workspace/.cargo".to_string(),
            read_only: false,
        };
        assert!(!is_restricted_breakout_mount(&mount));
    }

    #[test]
    fn append_git_config_env_sets_first_entry() {
        let mut env = HashMap::new();
        append_git_config_env(&mut env, "safe.directory", "/workspace");

        assert_eq!(env.get("GIT_CONFIG_COUNT").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("safe.directory")
        );
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_0").map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn append_git_config_env_appends_to_existing_entries() {
        let mut env = HashMap::from([
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "user.email".to_string()),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                "agent@example.com".to_string(),
            ),
        ]);

        append_git_config_env(&mut env, "safe.directory", "/workspace");

        assert_eq!(env.get("GIT_CONFIG_COUNT").map(String::as_str), Some("2"));
        assert_eq!(
            env.get("GIT_CONFIG_KEY_1").map(String::as_str),
            Some("safe.directory")
        );
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_1").map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn append_git_identity_env_defaults_sets_all_values() {
        let mut env = HashMap::new();

        append_git_identity_env_defaults(&mut env);

        assert_eq!(
            env.get("GIT_AUTHOR_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
    }

    #[test]
    fn append_git_identity_env_defaults_preserves_existing_values() {
        let mut env = HashMap::from([
            ("GIT_AUTHOR_NAME".to_string(), "Existing Author".to_string()),
            (
                "GIT_COMMITTER_EMAIL".to_string(),
                "existing@example.com".to_string(),
            ),
        ]);

        append_git_identity_env_defaults(&mut env);

        assert_eq!(
            env.get("GIT_AUTHOR_NAME").map(String::as_str),
            Some("Existing Author")
        );
        assert_eq!(
            env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_EMAIL").map(String::as_str),
            Some("existing@example.com")
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_run_user_arg_uses_workspace_owner_uid_gid() {
        use std::os::unix::fs::MetadataExt;

        let workspace_dir =
            std::env::temp_dir().join(format!("local-agent-docker-user-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");

        let metadata = std::fs::metadata(&workspace_dir).expect("workspace metadata");
        let expected = format!("{}:{}", metadata.uid(), metadata.gid());
        assert_eq!(docker_run_user_arg(&workspace_dir), Some(expected));

        let _ = std::fs::remove_dir_all(&workspace_dir);
    }
}

fn build_run_json_payload(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    outcome: &ExecutionOutcome,
    termination_reason: &TerminationReason,
    exit_code: Option<i32>,
    workspace_display: &str,
    transcript_dir_display: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": JOB_RUN_SCHEMA_NAME,
        "job_id": job.id,
        "work_item_id": job.work_item_id,
        "attempt": job.attempt,
        "mode": &runtime_spec.environment.execution_mode,
        "timeout_secs": runtime_spec.environment.timeout_secs,
        "container_image": &runtime_spec.environment.container_image,
        "outcome": outcome,
        "termination_reason": termination_reason,
        "exit_code": exit_code,
        "workspace": workspace_display,
        "transcript_dir": transcript_dir_display,
        "at": Utc::now(),
    })
}

fn planned_permission_tier_name(tier: &protocol::PlannedPermissionTier) -> &'static str {
    match tier {
        protocol::PlannedPermissionTier::DockerDefault => "docker_default",
    }
}

#[cfg(test)]
mod multi_repo_tests {
    use super::*;

    #[test]
    fn repo_name_from_url_handles_https_and_ssh() {
        assert_eq!(
            repo_name_from_url("https://github.com/reddit/runbooks.git"),
            "runbooks"
        );
        assert_eq!(
            repo_name_from_url("git@github.com:reddit/agent-hub.git"),
            "agent-hub"
        );
    }

    #[test]
    fn additional_repo_claims_include_candidates_when_primary_repo_missing() {
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let runtime_spec = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            github_capabilities: Vec::new(),
            repos: vec![protocol::PlannedRepoClaim {
                repo_url: "https://github.com/reddit/opencode.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: true,
                target_branch: None,
                claim_type: "read_only".to_string(),
                branch_name: None,
            }],
        };

        let claims = additional_repo_claims_for_mounts(&job, &runtime_spec);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].repo_url, "https://github.com/reddit/opencode.git");
    }

    #[test]
    fn additional_repo_claims_exclude_primary_repo_url_regardless_of_position() {
        let primary_url = "https://github.com/reddit/agent-hub.git";
        let candidate_url = "https://github.com/reddit/opencode.git";
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: Some(protocol::RepoSource {
                repo_url: primary_url.to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let runtime_spec = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            github_capabilities: Vec::new(),
            repos: vec![
                protocol::PlannedRepoClaim {
                    repo_url: candidate_url.to_string(),
                    base_ref: "origin/main".to_string(),
                    read_only: true,
                    target_branch: None,
                    claim_type: "read_only".to_string(),
                    branch_name: None,
                },
                protocol::PlannedRepoClaim {
                    repo_url: primary_url.to_string(),
                    base_ref: "origin/main".to_string(),
                    read_only: false,
                    target_branch: Some("agent-hub/test".to_string()),
                    claim_type: "workspace".to_string(),
                    branch_name: Some("agent-hub/test".to_string()),
                },
            ],
        };

        let claims = additional_repo_claims_for_mounts(&job, &runtime_spec);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].repo_url, candidate_url);
    }

    #[test]
    fn build_opencode_spawn_run_args_preserves_repo_mounts() {
        let invocation_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--user".to_string(),
            "1001:1001".to_string(),
            "-v".to_string(),
            "/host/work:/workspace:rw".to_string(),
            "-v".to_string(),
            "/host/repo:/repos/agent-hub-poc-test:ro".to_string(),
            "--entrypoint".to_string(),
            "sh".to_string(),
            "agent-hub/job-code:2026-08-18".to_string(),
            "-lc".to_string(),
            "opencode run".to_string(),
        ];

        let run_args = build_opencode_spawn_run_args(&invocation_args, "agent-hub-opencode-test")
            .expect("args");

        let repo_mount = "/host/repo:/repos/agent-hub-poc-test:ro".to_string();
        assert!(run_args.iter().any(|arg| arg == &repo_mount));
        assert!(
            run_args
                .windows(2)
                .any(|pair| pair[0] == "--user" && pair[1] == "1001:1001")
        );
        assert!(!run_args.iter().any(|arg| arg == "--rm"));
        assert_eq!(run_args[0], "run");
        assert!(run_args.iter().any(|arg| arg == "--name"));
        assert!(run_args.iter().any(|arg| arg == "agent-hub-opencode-test"));
        assert!(run_args.iter().any(|arg| arg == "-d"));
    }
}

#[cfg(test)]
mod triage_validation_tests {
    use super::*;

    fn triage_job() -> Job {
        let now = Utc::now();
        Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "triage".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: Some("worker".to_string()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("agent-image".to_string()),
            command: "agent".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({"work_item": {"workflow_id": "__triage__"}}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn triage_jobs_never_receive_remote_credentials() {
        let triage = serde_json::json!({"workflow_id": "__triage__"});
        let normal = serde_json::json!({"workflow_id": "implementation"});
        assert!(!job_allows_remote_credentials(&triage));
        assert!(job_allows_remote_credentials(&normal));
    }

    #[test]
    fn triage_rejects_runner_runtime_and_credential_provider_mounts() {
        let job = triage_job();
        let mut runtime = canonical_runtime_spec_for_job(&job);
        let mut runner = ResolvedRunnerSpec {
            kind: ResolvedRunnerKind::Generic,
            command: "agent".to_string(),
            args: Vec::new(),
            file_mounts: vec![protocol::orchestration::PlannedFileMount {
                host_path: "/home/user/.ssh".to_string(),
                container_path: "/credentials".to_string(),
                read_only: true,
            }],
            requires_credentials: Vec::new(),
        };
        assert!(
            validate_triage_mount_and_credential_requests(
                true,
                &runner,
                &runtime,
                &LocalAgentConfig::default(),
            )
            .is_err()
        );

        runner.file_mounts.clear();
        runner.requires_credentials.push("github".to_string());
        assert!(
            validate_triage_mount_and_credential_requests(
                true,
                &runner,
                &runtime,
                &LocalAgentConfig::default(),
            )
            .is_err()
        );

        runner.requires_credentials.clear();
        runtime
            .environment
            .file_mounts
            .push(protocol::orchestration::PlannedFileMount {
                host_path: "/var/run/docker.sock".to_string(),
                container_path: "/var/run/docker.sock".to_string(),
                read_only: false,
            });
        assert!(
            validate_triage_mount_and_credential_requests(
                true,
                &runner,
                &runtime,
                &LocalAgentConfig::default(),
            )
            .is_err()
        );

        runtime.environment.file_mounts.clear();
        runtime.repos.push(protocol::PlannedRepoClaim {
            repo_url: "https://example.invalid/arbitrary/repo".to_string(),
            base_ref: "origin/main".to_string(),
            read_only: true,
            target_branch: None,
            claim_type: "read_only".to_string(),
            branch_name: None,
        });
        assert!(
            validate_triage_mount_and_credential_requests(
                true,
                &runner,
                &runtime,
                &LocalAgentConfig::default(),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn triage_docker_access_controls_add_no_mcp_gh_or_provider_material() {
        let job = triage_job();
        let mut config = LocalAgentConfig::default();
        config.mcp_bridges.push(crate::mcp_bridge::McpBridgeConfig {
            name: "forbidden".to_string(),
            host_endpoint: crate::mcp_bridge::McpEndpoint::Unix {
                path: "/run/forbidden.sock".to_string(),
            },
            container_socket_path: Some("/run/mcp.sock".to_string()),
        });
        let mut args = Vec::new();
        let mut env = HashMap::new();
        let bridges = configure_mcp_bridges_for_docker(&mut args, &mut env, &config, &job, None)
            .await
            .expect("triage should skip MCP configuration");
        assert!(bridges.is_empty());
        assert!(args.is_empty());
        assert!(env.is_empty());

        assert!(args.is_empty());
        assert!(env.is_empty());
    }

    #[test]
    fn triage_success_allows_any_concrete_repo_when_repo_catalog_missing() {
        let work_item_context = serde_json::json!({
            "workflow_id": "__triage__"
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/agent-hub".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        validate_triage_repo_rules(&work_item_context, &output)
            .expect("should pass without repo_catalog constraint");
    }

    #[test]
    fn triage_success_requires_repo_to_match_repo_catalog_when_present() {
        let work_item_context = serde_json::json!({
            "workflow_id": "__triage__",
            "repo_catalog": [
                { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" },
                { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" }
            ]
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/agent-hub".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        let err = validate_triage_repo_rules(&work_item_context, &output)
            .expect_err("mismatched repo_url should fail");
        assert!(
            err.to_string()
                .contains("must match one of work_item.repo_catalog"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn triage_success_accepts_repo_present_in_repo_catalog() {
        let work_item_context = serde_json::json!({
            "workflow_id": "__triage__",
            "repo_catalog": [
                { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" },
                { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" }
            ]
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/opencode".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        validate_triage_repo_rules(&work_item_context, &output)
            .expect("matching repo_url should pass");
    }
}

#[cfg(test)]
mod job_context_contract_tests {
    use super::*;

    #[test]
    fn work_item_contract_includes_owner_github_login() {
        let job_context = serde_json::json!({
            "work_item": {
                "id": "w1",
                "owner_github_login": "owner-dev"
            }
        });

        let work_item = work_item_contract_from_job_context(&job_context);
        assert_eq!(
            work_item["owner_github_login"],
            serde_json::json!("owner-dev")
        );
    }
}

const JOB_HEARTBEAT_SECS: u64 = 20;
const TRANSCRIPT_CLEANUP_INTERVAL_SECS: u64 = 60 * 60;
const REPO_CACHE_FETCH_INTERVAL_SECS: u64 = 5 * 60;
const DEFAULT_DOCKER_MEMORY_LIMIT: &str = "8g";
const DEFAULT_DOCKER_CPU_LIMIT: &str = "4";

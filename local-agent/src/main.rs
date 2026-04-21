use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use clap::{Parser, ValueEnum};
use protocol::{
    AgentOutput, ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming,
    JOB_RUN_SCHEMA_NAME, Job, JobEvent, JobEventKind, JobResult, JobState, NetworkResourceKind,
    NetworkResourceRequest, RegisterWorkerRequest, Secret, TerminationReason, WorkerState,
    canonical_runtime_spec_for_job, validate_agent_output_next_event,
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

use artifacts::{
    AdditionalRepoMountView, AgentContextWriteParams, GITHUB_DEFAULT_HOST,
    collect_structured_transcript_artifacts, display_paths_for_runtime, file_len_u64,
    normalize_github_repo_slug_with_allowlist, parse_github_pr_from_output,
    parse_github_repo_url, work_item_contract_from_job_context,
    workflow_valid_next_events_from_job_context, write_agent_context_file,
};
use client::{
    Client, GrpcClient, HttpClient, JobUpdateOutcome, is_worker_not_found_error,
    start_grpc_event_stream,
};
use credential_provider::{ResolvedCredentials, resolve_all};
use egress::{EgressManager, llm_provider_from_env};
use egress_config::{
    LocalAgentConfig, ManagedRunnerKind, ManagedRunnerSpec, load_local_agent_config,
};
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

    #[arg(long, value_enum, default_values_t = vec![ModeArg::Raw, ModeArg::Docker])]
    mode: Vec<ModeArg>,

    #[arg(long, default_value = "./local-agent-data")]
    data_dir: PathBuf,

    #[arg(long, env = "ARTIFACT_STORE_ROOT")]
    artifact_store_root: Option<PathBuf>,

    /// Optional local-agent JSON config override (default: ~/.config/agent-hub/local-agent.json)
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
enum ModeArg {
    Raw,
    Docker,
}

#[derive(Clone, ValueEnum)]
enum TransportKind {
    Http,
    Grpc,
}

impl From<ModeArg> for ExecutionMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Raw => ExecutionMode::Raw,
            ModeArg::Docker => ExecutionMode::Docker,
        }
    }
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

fn ensure_repo_cloned(url: &str, cache_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create repo cache directory {}", cache_dir.display()))?;

    let repo_name = repo_name_from_url(url);
    let repo_dir = cache_dir.join(&repo_name);

    if repo_dir.exists() {
        if !repo_dir.is_dir() {
            anyhow::bail!(
                "repo cache target '{}' exists but is not a directory",
                repo_dir.display()
            );
        }
        if !repo_dir.join(".git").is_dir() {
            anyhow::bail!(
                "repo cache target '{}' exists but is not a git repository",
                repo_dir.display()
            );
        }
        return Ok(repo_dir);
    }

    let clone = std::process::Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(&repo_dir)
        .output()
        .with_context(|| {
            format!(
                "run git clone '{}' into '{}'",
                url,
                repo_dir.to_string_lossy()
            )
        })?;
    if !clone.status.success() {
        let stderr = String::from_utf8_lossy(&clone.stderr);
        anyhow::bail!(
            "git clone failed for '{}' into '{}': {}",
            url,
            repo_dir.display(),
            stderr.trim()
        );
    }

    Ok(repo_dir)
}

fn validate_local_repo_dir(raw_dir: &str) -> anyhow::Result<PathBuf> {
    let expanded = expand_home_path(raw_dir)
        .with_context(|| format!("expand triage_repos entry '{raw_dir}'"))?;
    let host_dir = resolve_to_absolute(&expanded).with_context(|| {
        format!(
            "resolve triage_repos entry '{}' ({})",
            raw_dir,
            expanded.display()
        )
    })?;

    if !host_dir.exists() {
        anyhow::bail!(
            "triage_repos entry '{}' resolved to '{}' but does not exist",
            raw_dir,
            host_dir.display()
        );
    }

    if !host_dir.is_dir() {
        anyhow::bail!(
            "triage_repos entry '{}' resolved to '{}' but is not a directory",
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
    std::fs::create_dir_all(repo_cache_dir)
        .with_context(|| format!("create repo cache directory {}", repo_cache_dir.display()))?;

    let mut remote_repos = Vec::new();
    for raw_entry in &local_agent_config.triage_repos {
        match classify_repo_source(raw_entry) {
            RepoSourceKind::LocalDirectory(raw_dir) => {
                validate_local_repo_dir(&raw_dir)?;
            }
            RepoSourceKind::GitRemote(url) => {
                ensure_repo_cloned(&url, repo_cache_dir)?;
                remote_repos.push(url);
            }
        }
    }

    Ok(remote_repos)
}

fn configured_repo_discovery_mounts(
    local_agent_config: &LocalAgentConfig,
    repo_cache_dir: &Path,
) -> anyhow::Result<Vec<protocol::orchestration::PlannedFileMount>> {
    let mut mounts = Vec::new();

    for raw_entry in &local_agent_config.triage_repos {
        let host_dir = match classify_repo_source(raw_entry) {
            RepoSourceKind::LocalDirectory(raw_dir) => validate_local_repo_dir(&raw_dir)?,
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
    fn parse_github_ssh_repo_slug() {
        let allowlist = LocalAgentConfig::default().github_host_allowlist();
        assert_eq!(
            normalize_github_repo_slug_with_allowlist(
                "git@github.com:reddit/agent-hub.git",
                &allowlist,
            ),
            Some("reddit/agent-hub".to_string())
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
    fn parse_github_repo_url_extracts_host_and_slug_for_ssh() {
        let parsed = parse_github_repo_url("git@github.snooguts.net:reddit/agent-hub.git")
            .expect("parse ssh GHE URL");
        assert_eq!(parsed.host, "github.snooguts.net");
        assert_eq!(parsed.slug, "reddit/agent-hub");
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
        cfg.github_hosts = vec![
            "github.com".to_string(),
            "github.snooguts.net".to_string(),
        ];
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
    fn append_gh_config_mount_sets_gh_host_for_ghe_job_override() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        append_gh_config_mount_for_toolchains(
            &mut args,
            &mut env,
            &["gh".to_string()],
            Some("github.snooguts.net"),
        );
        assert_eq!(
            env.get("GH_HOST").map(String::as_str),
            Some("github.snooguts.net"),
            "GH_HOST should be set in docker_env when the job repo is on GHE",
        );
    }

    #[test]
    fn append_gh_config_mount_skips_gh_host_for_default_host_override() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        append_gh_config_mount_for_toolchains(
            &mut args,
            &mut env,
            &["gh".to_string()],
            Some("github.com"),
        );
        assert!(
            !env.contains_key("GH_HOST"),
            "GH_HOST should not be forced when the job repo is on github.com",
        );
    }

    #[test]
    fn append_gh_config_mount_skips_gh_host_when_no_gh_toolchain() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        append_gh_config_mount_for_toolchains(
            &mut args,
            &mut env,
            &["git".to_string()],
            Some("github.snooguts.net"),
        );
        assert!(
            !env.contains_key("GH_HOST"),
            "GH_HOST should only be set when the gh toolchain is requested",
        );
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
    fn run_raw_wait_strategy_does_not_depend_on_second_command_spawn() {
        // Regression guard for no-timeout branch: command must execute once.
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!("run-raw-once-{}", uuid::Uuid::new_v4()));
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
                execution_mode: ExecutionMode::Raw,
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

            let status = run_raw(
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
            .expect("run_raw");

            assert!(matches!(status, ExecutionStatus::Exited(0)));
            let n = std::fs::read_to_string(&counter).expect("counter exists");
            assert_eq!(n.trim(), "1");

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn run_raw_output_validation_early_exit_stops_lingering_process_after_valid_output() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!(
                "run-raw-validate-early-exit-{}",
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
                execution_mode: ExecutionMode::Raw,
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
            let status = run_raw_with_output_validation_early_exit(
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
            .expect("run_raw_with_output_validation_early_exit");

            assert!(matches!(status, ExecutionStatus::Exited(0)));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "expected early-exit before lingering sleep"
            );

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn run_raw_output_validation_early_exit_does_not_accept_invalid_output() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!(
                "run-raw-validate-invalid-output-{}",
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
                execution_mode: ExecutionMode::Raw,
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

            let status = run_raw_with_output_validation_early_exit(
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
            .expect("run_raw_with_output_validation_early_exit");

            assert!(matches!(status, ExecutionStatus::TimedOut));

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn run_raw_cancel_signal_kills_process_and_returns_cancelled() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root =
                std::env::temp_dir().join(format!("run-raw-cancel-{}", uuid::Uuid::new_v4()));
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
                execution_mode: ExecutionMode::Raw,
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

            let run = run_raw(
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

            let status = run.await.expect("run_raw");
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
            execution_mode: ExecutionMode::Raw,
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
            execution_mode: ExecutionMode::Raw,
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
            execution_mode: ExecutionMode::Raw,
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
            execution_mode: ExecutionMode::Raw,
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
            execution_mode: ExecutionMode::Raw,
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
            execution_mode: ExecutionMode::Raw,
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
                network: None,
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
                network: None,
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
                network: None,
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
                network: None,
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
                network: Some("internet".to_string()),
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
                execution_mode: ExecutionMode::Raw,
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
                network: None,
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
        };
        let mut ctx = serde_json::json!({});
        protocol::project_runtime_spec_into_job_context(&mut ctx, &runtime);

        let parsed = protocol::canonical_runtime_spec_for_job_effective(&Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "s".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
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
        assert_eq!(parsed.permissions.can_access_network, false);
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
            execution_mode: ExecutionMode::Raw,
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
                tier: protocol::PlannedEnvironmentTier::Raw,
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::RawCompatible,
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
                .get("job_events_jsonl")
                .and_then(serde_json::Value::as_str),
            Some(
                transcript_dir
                    .join("job-events.jsonl")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            paths.get("stdout_log").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("stdout.log").to_string_lossy().as_ref())
        );
        assert_eq!(
            paths.get("stderr_log").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("stderr.log").to_string_lossy().as_ref())
        );
        assert_eq!(
            paths.get("run_json").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("run.json").to_string_lossy().as_ref())
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
                network: None,
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
            execution_mode: ExecutionMode::Raw,
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
                tier: protocol::PlannedEnvironmentTier::Raw,
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::RawCompatible,
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
            execution_mode: ExecutionMode::Raw,
            container_image: Some("legacy:image".into()),
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
                network: None,
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
    let modes = cli
        .mode
        .into_iter()
        .map(ExecutionMode::from)
        .collect::<Vec<_>>();

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
            local_agent_config.resolve_default_agent(cli.default_agent.as_deref());
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
    data_dir: &Path,
    repos_root: &Path,
    worktrees_root: &Path,
    egress_manager: &EgressManager,
    event_publisher: Option<&JobEventPublisher>,
    cancel_rx: watch::Receiver<bool>,
) -> anyhow::Result<JobResult> {
    let transcript_dir = resolve_to_absolute(Path::new(transcript_dir))
        .with_context(|| format!("resolve transcript dir {}", transcript_dir))?
        .to_string_lossy()
        .to_string();
    let transcript_dir = transcript_dir.as_str();

    std::fs::create_dir_all(transcript_dir)?;

    let local_agent_config = egress_manager.config();
    let environment_table =
        EnvironmentValidationTable::from_local_agent_config(local_agent_config)?;
    let execution_kind = job_execution_kind_from_context(&job.context);
    let managed_operation = managed_operation_from_job_context(&job.context);
    let environment_runtime = environment_runtime_from_job_context(&job.context);
    let resolved_runner = resolve_runner_spec_for_job(
        job,
        execution_kind,
        managed_operation.as_deref(),
        environment_runtime.as_deref(),
        local_agent_config,
    )?;

    let (runtime_spec, runtime_spec_source) = resolve_runtime_spec_and_source(job);
    let mut runtime_spec = runtime_spec;
    if runtime_spec.environment.container_image.is_none() {
        runtime_spec.environment.container_image = resolved_runner.container_image.clone();
    }
    runtime_spec
        .environment
        .file_mounts
        .extend(resolved_runner.file_mounts.clone());
    runtime_spec
        .environment
        .file_mounts
        .extend(configured_repo_discovery_mounts(
            local_agent_config,
            &data_dir.join("repo-cache"),
        )?);
    let local_egress_plan = resolve_local_agent_egress_plan(&runtime_spec, local_agent_config)?;
    let runtime_source_label = runtime_spec_source.label();
    match runtime_spec_source {
        RuntimeSpecSource::CanonicalContext => {
            eprintln!(
                "info: job {} using {} (version={}, mode={}, timeout_secs={:?}, network={:?}, mounts={}, toolchains={})",
                job.id,
                runtime_source_label,
                runtime_spec.runtime_version,
                runtime_spec.environment.execution_mode,
                runtime_spec.environment.timeout_secs,
                runtime_spec.environment.network,
                runtime_spec.environment.file_mounts.len(),
                runtime_spec.environment.toolchains.len()
            );
        }
        RuntimeSpecSource::LegacyJobFields => {
            eprintln!(
                "WARNING: job {} falling back to {} (mode={}, timeout_secs={:?}, execution_kind={})",
                job.id,
                runtime_source_label,
                runtime_spec.environment.execution_mode,
                runtime_spec.environment.timeout_secs,
                execution_kind.as_str()
            );
        }
    }

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
    let work_item_context = work_item_contract_from_job_context(&job.context);

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
                    runtime_spec.environment.network
                ),
            )
            .await;
        let execution_status = match runtime_spec.environment.execution_mode {
            ExecutionMode::Raw => {
                if matches!(local_egress_plan.tier, Some(LocalAgentEgressTier::Llm | LocalAgentEgressTier::Build))
                {
                    anyhow::bail!(
                        "restricted egress tiers are only supported for Docker execution mode; raw mode cannot enforce restricted egress"
                    );
                }
                if !runtime_spec.permissions.can_access_network {
                    let warning = "WARNING: network restriction is not enforced in raw mode";
                    eprintln!("{warning}");
                    job_event_sink
                        .emit(job.id, pb::JobEventKind::JobEventSystem, warning.to_string())
                        .await;
                }
                if !runtime_spec.permissions.can_write_workspace {
                    let warning =
                        "WARNING: workspace write restriction is not enforced in raw mode";
                    eprintln!("{warning}");
                    job_event_sink
                        .emit(job.id, pb::JobEventKind::JobEventSystem, warning.to_string())
                        .await;
                }
                run_raw(
                    &resolved_job,
                    &workspace_dir,
                    &transcript_jsonl_path,
                    &stdout_path,
                    &stderr_path,
                    &env,
                    runtime_spec.environment.timeout_secs,
                    Some(&job_event_sink),
                    Some(cancel_rx.clone()),
                )
                .await?
            }
            ExecutionMode::Docker => {
                match &resolved_runner.kind {
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
                            workflow_valid_next_events_from_job_context(&job.context)
                                .unwrap_or_default(),
                            work_item_context.clone(),
                            &environment_table,
                            Some(&job_event_sink),
                            Some(cancel_rx.clone()),
                        )
                        .await?
                    }
                    ResolvedRunnerKind::Cursor(cursor) => {
                        run_docker_cursor_with_validation_retries(
                            &resolved_job,
                            local_agent_config,
                            cursor,
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
                            workflow_valid_next_events_from_job_context(&job.context)
                                .unwrap_or_default(),
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
                }
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
                (ExecutionOutcome::Cancelled, TerminationReason::Cancelled, None)
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
                    Some(artifact_store_relative_path(transcript_rel_dir, "output.json"))
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

#[derive(Debug, Clone, Copy)]
enum RuntimeSpecSource {
    CanonicalContext,
    LegacyJobFields,
}

impl RuntimeSpecSource {
    fn label(self) -> &'static str {
        match self {
            Self::CanonicalContext => "canonical-runtime-spec",
            Self::LegacyJobFields => "legacy-job-fields-fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobExecutionKind {
    Agent,
    Command,
    Managed,
}

impl JobExecutionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Managed => "managed",
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedRunnerSpec {
    kind: ResolvedRunnerKind,
    command: String,
    args: Vec<String>,
    container_image: Option<String>,
    file_mounts: Vec<protocol::orchestration::PlannedFileMount>,
    requires_credentials: Vec<String>,
}

#[derive(Debug, Clone)]
enum ResolvedRunnerKind {
    Generic,
    Opencode(OpencodeManagedSpec),
    Cursor(CursorManagedSpec),
}

#[derive(Debug, Clone)]
struct OpencodeManagedSpec {
    max_validation_attempts: u32,
}

#[derive(Debug, Clone)]
struct CursorManagedSpec {
    max_validation_attempts: u32,
    default_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalAgentEgressTier {
    Llm,
    Build,
    Full,
}

impl LocalAgentEgressTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
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
    if runtime_spec.environment.network_resources.is_none()
        && let Some(raw_network) = runtime_spec.environment.network.as_deref()
        && !matches!(raw_network, "none" | "restricted" | "full")
    {
        anyhow::bail!(
            "legacy network label '{}' is unsupported; expected one of: none, restricted, full",
            raw_network
        );
    }

    let request = runtime_spec
        .environment
        .network_resources
        .clone()
        .or_else(|| {
            protocol::network_request_from_legacy_label(runtime_spec.environment.network.as_deref())
        });

    let Some(request) = request else {
        return Ok(LocalAgentEgressPlan {
            network_none: !runtime_spec.permissions.can_access_network,
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

fn resolve_runtime_spec_and_source(
    job: &Job,
) -> (protocol::CanonicalRuntimeSpec, RuntimeSpecSource) {
    if let Some(runtime_spec) = protocol::runtime_spec_from_job_context(&job.context) {
        if runtime_spec.runtime_version == "v2" {
            return (runtime_spec, RuntimeSpecSource::CanonicalContext);
        }
        let mut fallback = canonical_runtime_spec_for_job(job);
        fallback.runtime_version = runtime_spec.runtime_version;
        merge_missing_repo_claims(&mut fallback.repos, runtime_spec.repos);
        (fallback, RuntimeSpecSource::LegacyJobFields)
    } else {
        let mut fallback = canonical_runtime_spec_for_job(job);
        merge_missing_repo_claims(
            &mut fallback.repos,
            repo_claims_from_runtime_spec_context_value(&job.context),
        );
        (fallback, RuntimeSpecSource::LegacyJobFields)
    }
}

fn merge_missing_repo_claims(
    destination: &mut Vec<protocol::PlannedRepoClaim>,
    candidates: Vec<protocol::PlannedRepoClaim>,
) {
    if candidates.is_empty() {
        return;
    }

    let mut seen = destination
        .iter()
        .map(|claim| claim.repo_url.trim().to_string())
        .filter(|repo_url| !repo_url.is_empty())
        .collect::<HashSet<_>>();

    for claim in candidates {
        let repo_url = claim.repo_url.trim().to_string();
        if repo_url.is_empty() || !seen.insert(repo_url) {
            continue;
        }
        destination.push(claim);
    }
}

fn repo_claims_from_runtime_spec_context_value(
    context: &serde_json::Value,
) -> Vec<protocol::PlannedRepoClaim> {
    context
        .as_object()
        .and_then(|obj| obj.get("canonical_runtime_spec"))
        .and_then(serde_json::Value::as_object)
        .and_then(|runtime| runtime.get("repos"))
        .and_then(serde_json::Value::as_array)
        .map(|repos| {
            repos
                .iter()
                .filter_map(repo_claim_from_runtime_context_entry)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn repo_claim_from_runtime_context_entry(
    entry: &serde_json::Value,
) -> Option<protocol::PlannedRepoClaim> {
    let obj = entry.as_object()?;
    let repo_url = obj
        .get("repo_url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let base_ref = obj
        .get("base_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("origin/main")
        .to_string();
    let read_only = obj
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let target_branch = obj
        .get("target_branch")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let claim_type = obj
        .get("claim_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if read_only {
                "read_only".to_string()
            } else {
                "workspace".to_string()
            }
        });
    let branch_name = obj
        .get("branch_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    Some(protocol::PlannedRepoClaim {
        repo_url,
        base_ref,
        read_only,
        target_branch,
        claim_type,
        branch_name,
    })
}

fn job_execution_kind_from_context(context: &serde_json::Value) -> JobExecutionKind {
    let raw = context
        .get("workflow")
        .and_then(|workflow| workflow.get("job_execution_kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("command");
    match raw {
        "agent" => JobExecutionKind::Agent,
        "managed" => JobExecutionKind::Managed,
        _ => JobExecutionKind::Command,
    }
}

fn managed_operation_from_job_context(context: &serde_json::Value) -> Option<String> {
    context
        .get("workflow")
        .and_then(|workflow| workflow.get("managed_operation"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn environment_runtime_from_job_context(context: &serde_json::Value) -> Option<String> {
    let from_env_obj = context
        .get("environment")
        .and_then(|env| env.get("runtime"))
        .and_then(serde_json::Value::as_str);
    let from_top_level = context
        .get("environment_runtime")
        .and_then(serde_json::Value::as_str);
    from_env_obj
        .or(from_top_level)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_runner_spec_for_job(
    job: &Job,
    execution_kind: JobExecutionKind,
    managed_operation: Option<&str>,
    environment_runtime: Option<&str>,
    config: &LocalAgentConfig,
) -> anyhow::Result<ResolvedRunnerSpec> {
    match execution_kind {
        JobExecutionKind::Command => Ok(ResolvedRunnerSpec {
            kind: ResolvedRunnerKind::Generic,
            command: job.command.clone(),
            args: job.args.clone(),
            container_image: None,
            file_mounts: Vec::new(),
            requires_credentials: Vec::new(),
        }),
        JobExecutionKind::Agent => {
            if let Some(runtime) = environment_runtime
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "auto")
            {
                if let Some(spec) = config.managed_runners.get(runtime) {
                    return resolved_runner_spec_from_config(spec, Some(runtime));
                }
                eprintln!(
                    "warning: environment.runtime={runtime} but no matching managed_runners entry; falling back to default_agent"
                );
            }
            resolved_runner_spec_from_config(&config.default_agent, Some("default_agent"))
        }
        JobExecutionKind::Managed => {
            let operation = managed_operation.ok_or_else(|| {
                anyhow::anyhow!(
                    "managed job {} missing workflow.managed_operation context",
                    job.id
                )
            })?;
            let spec = config
                .managed_runner_for_operation(operation)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed job {} references unknown managed operation '{}'",
                        job.id,
                        operation
                    )
                })?;
            resolved_runner_spec_from_config(spec, Some(operation))
        }
    }
}

fn resolved_runner_spec_from_config(
    spec: &ManagedRunnerSpec,
    label: Option<&str>,
) -> anyhow::Result<ResolvedRunnerSpec> {
    if spec.command.trim().is_empty() {
        let target = label.unwrap_or("managed runner");
        anyhow::bail!("{target} command must not be empty");
    }

    Ok(ResolvedRunnerSpec {
        kind: resolved_runner_kind_from_spec(spec),
        command: spec.command.clone(),
        args: spec.args.clone(),
        container_image: spec.container_image.clone(),
        file_mounts: spec
            .file_mounts
            .iter()
            .map(|mount| protocol::orchestration::PlannedFileMount {
                host_path: mount.host_path.clone(),
                container_path: mount.container_path.clone(),
                read_only: mount.read_only,
            })
            .collect(),
        requires_credentials: spec.requires_credentials.clone(),
    })
}

fn resolved_runner_kind_from_spec(spec: &ManagedRunnerSpec) -> ResolvedRunnerKind {
    match spec.kind {
        ManagedRunnerKind::Opencode => ResolvedRunnerKind::Opencode(OpencodeManagedSpec {
            max_validation_attempts: spec.opencode.max_validation_attempts,
        }),
        ManagedRunnerKind::Cursor => ResolvedRunnerKind::Cursor(CursorManagedSpec {
            max_validation_attempts: spec.cursor.max_validation_attempts,
            default_model: spec.cursor.default_model.clone(),
        }),
        ManagedRunnerKind::Generic => {
            if runner_spec_looks_like_cursor(spec) {
                ResolvedRunnerKind::Cursor(CursorManagedSpec {
                    max_validation_attempts: spec.cursor.max_validation_attempts,
                    default_model: spec.cursor.default_model.clone(),
                })
            } else if runner_spec_looks_like_opencode(spec) {
                ResolvedRunnerKind::Opencode(OpencodeManagedSpec {
                    max_validation_attempts: spec.opencode.max_validation_attempts,
                })
            } else {
                ResolvedRunnerKind::Generic
            }
        }
    }
}

fn runner_spec_looks_like_opencode(spec: &ManagedRunnerSpec) -> bool {
    if spec.command.trim() == "opencode" {
        return true;
    }
    spec.args
        .iter()
        .any(|arg| arg.contains("opencode run") || arg.contains("opencode\trun"))
}

fn runner_spec_looks_like_cursor(spec: &ManagedRunnerSpec) -> bool {
    let cmd = spec.command.trim();
    if cmd == "cursor-agent" || cmd == "agent" {
        return true;
    }
    spec.args.iter().any(|arg| {
        arg.contains("cursor-agent ") || arg.contains("agent -p") || arg.contains("agent --print")
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_raw(
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
    run_raw_with_mode(
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
async fn run_raw_with_output_validation_early_exit(
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

    eprintln!("info: spawning command: {} {:?}", &job.command, &job.args);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn command: {} {:?}", &job.command, &job.args))?;

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
async fn run_raw_with_mode(
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
        "info: run_raw_with_mode spawning: cmd={} args_count={} cwd={} cwd_exists={}",
        &job.command,
        job.args.len(),
        workspace_dir.display(),
        workspace_dir.exists()
    );
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn '{}' (args={}) in cwd={}",
            &job.command,
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
        Some(LocalAgentEgressTier::Llm) | Some(LocalAgentEgressTier::Build) => {
            local_egress_plan.tier
        }
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
        args.push(format!("--network={}", restricted_runtime.network_name));

        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            docker_env.remove(key);
        }
        docker_env.insert(
            "HTTP_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "HTTPS_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("NO_PROXY".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "http_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "https_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("no_proxy".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            restricted_runtime.tier_hosts.join(","),
        );

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

    let job_gh_host = job_gh_host_override(job, local_agent_config);
    append_gh_config_mount_for_toolchains(
        &mut args,
        &mut docker_env,
        &runtime_spec.environment.toolchains,
        job_gh_host.as_deref(),
    );
    let resolved_credentials = resolve_configured_credentials(local_agent_config).await?;
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
    .await;
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
        &docker_job.command,
        docker_job.args.join(" ")
    );
    let run_result = run_raw(
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
    .await;
    for bridge in &mcp_bridges {
        bridge.stop();
    }
    cleanup_credential_temp_files_best_effort(&resolved_credentials.temp_files);
    run_result
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
        run_raw_with_output_validation_early_exit(
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
        run_raw_with_output_validation_early_exit(
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
async fn run_docker_cursor_with_validation_retries(
    job: &Job,
    local_agent_config: &LocalAgentConfig,
    cursor: &CursorManagedSpec,
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

    run_cursor_validation_loop(
        job,
        cursor,
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
async fn run_cursor_validation_loop(
    job: &Job,
    cursor: &CursorManagedSpec,
    invocation: &DockerRuntimeInvocation,
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    environment_table: &EnvironmentValidationTable,
    events: Option<&JobEventSink>,
    cancel_rx: Option<watch::Receiver<bool>>,
) -> anyhow::Result<ExecutionStatus> {
    let _ = cursor.default_model.as_ref();
    let max_attempts = cursor.max_validation_attempts.max(1);
    let _ = std::fs::remove_file(output_path);

    let container_name = format!("agent-hub-cursor-{}", job.id.simple());
    let mut attempt: u32 = 1;
    let mut continuation_prompt: Option<String> = None;
    let shell_prelude = cursor_shell_prelude(job);

    spawn_cursor_runtime_container(invocation, &container_name).await?;

    loop {
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    format!(
                        "cursor validation attempt {attempt}/{max_attempts} ({})",
                        if continuation_prompt.is_none() {
                            "run"
                        } else {
                            "agent --continue"
                        }
                    ),
                )
                .await;
        }

        let status = run_docker_cursor_attempt(
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
            if let Err(error) = cleanup_cursor_container(&container_name).await {
                eprintln!(
                    "warning: failed to cleanup cursor container {}: {error:#}",
                    container_name
                );
            }
            return Ok(status);
        }

        if !matches!(status, ExecutionStatus::Exited(0)) {
            if let Err(error) = cleanup_cursor_container(&container_name).await {
                eprintln!(
                    "warning: failed to cleanup cursor container {}: {error:#}",
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
                            format!("cursor output validation passed on attempt {attempt}"),
                        )
                        .await;
                }
                if let Err(error) = cleanup_cursor_container(&container_name).await {
                    eprintln!(
                        "warning: failed to cleanup cursor container {}: {error:#}",
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
                                    "cursor output validation failed after {attempt} attempts: {validation_error}"
                                ),
                            )
                            .await;
                    }
                    if let Err(error) = cleanup_cursor_container(&container_name).await {
                        eprintln!(
                            "warning: failed to cleanup cursor container {}: {error:#}",
                            container_name
                        );
                    }
                    anyhow::bail!(
                        "cursor validation failed after {attempt} attempts: {validation_error}"
                    );
                }

                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!(
                                "cursor output validation failed (attempt {attempt}/{max_attempts}): {validation_error}; retrying with agent --continue"
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
async fn run_docker_cursor_attempt(
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
    let cursor_cmd = if let Some(prompt) = continuation_prompt {
        build_cursor_continue_script(shell_prelude, prompt)
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
                cursor_cmd,
            ]
        } else {
            let mut exec_args = vec!["exec".to_string(), container_name.to_string()];
            exec_args.push(job.command.clone());
            exec_args.extend(job.args.clone());
            exec_args
        },
        ..job.clone()
    };

    run_raw_with_output_validation_early_exit(
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
        append_logs,
    )
    .await
}

async fn spawn_cursor_runtime_container(
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
            "failed to start cursor runtime container {}: {}",
            container_name,
            status
        )
    }
}

fn cursor_shell_prelude(job: &Job) -> String {
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

fn build_cursor_continue_script(shell_prelude: &str, prompt: &str) -> String {
    let mut script = String::new();
    if !shell_prelude.is_empty() {
        script.push_str(shell_prelude);
        script.push_str("; ");
    }
    script.push_str("cat <<'AGENT_HUB_RETRY_PROMPT' >/tmp/agent-hub-retry-prompt.txt\n");
    script.push_str(prompt);
    script.push_str(
        "\nAGENT_HUB_RETRY_PROMPT\nagent --continue -p --force --trust < /tmp/agent-hub-retry-prompt.txt",
    );
    script
}

async fn cleanup_cursor_container(container_name: &str) -> anyhow::Result<()> {
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
        Some(LocalAgentEgressTier::Llm) | Some(LocalAgentEgressTier::Build) => {
            local_egress_plan.tier
        }
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
        args.push(format!("--network={}", restricted_runtime.network_name));

        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            docker_env.remove(key);
        }
        docker_env.insert(
            "HTTP_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "HTTPS_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("NO_PROXY".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "http_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "https_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("no_proxy".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            restricted_runtime.tier_hosts.join(","),
        );

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

    let job_gh_host = job_gh_host_override(job, local_agent_config);
    append_gh_config_mount_for_toolchains(
        &mut args,
        &mut docker_env,
        &runtime_spec.environment.toolchains,
        job_gh_host.as_deref(),
    );
    let resolved_credentials = resolve_configured_credentials(local_agent_config).await?;
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
    .await;
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
) -> Vec<McpBridgeInstance> {
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
                let warning = format!(
                    "warning: failed to start MCP bridge '{}' ({error:#}); continuing without it",
                    bridge_config.name
                );
                eprintln!("{warning}");
                if let Some(events) = events {
                    events
                        .emit(job.id, pb::JobEventKind::JobEventSystem, warning)
                        .await;
                }
            }
        }
    }

    bridges
}

async fn resolve_configured_credentials(
    local_agent_config: &LocalAgentConfig,
) -> anyhow::Result<ResolvedCredentials> {
    if local_agent_config.credential_providers.is_empty() {
        return Ok(ResolvedCredentials::default());
    }

    let mut provider_names = local_agent_config
        .credential_providers
        .keys()
        .collect::<Vec<_>>();
    provider_names.sort_unstable();

    let providers = provider_names
        .into_iter()
        .filter_map(|name| local_agent_config.credential_providers.get(name))
        .cloned()
        .collect::<Vec<_>>();
    resolve_all(&providers).await
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

const GH_CONFIG_CONTAINER_DIR: &str = "/gh-config";
const GH_DEFAULT_HOST: &str = GITHUB_DEFAULT_HOST;

/// Resolve the GitHub host for `job` from its repo URL, but only when that
/// host appears in the configured `github_hosts` allowlist (see
/// [`LocalAgentConfig::github_host_allowlist`]). Returns `None` when the job
/// has no repo, the URL doesn't look like a GitHub host, or the host isn't
/// allowlisted — in those cases callers should fall back to the local-agent's
/// host-level `GH_HOST` (or the gh default).
fn job_gh_host_override(job: &Job, local_agent_config: &LocalAgentConfig) -> Option<String> {
    let repo = job.repo.as_ref()?;
    let parsed = parse_github_repo_url(&repo.repo_url)?;
    let allowlist = local_agent_config.github_host_allowlist();
    if allowlist
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&parsed.host))
    {
        Some(parsed.host)
    } else {
        None
    }
}

fn append_gh_config_mount_for_toolchains(
    args: &mut Vec<String>,
    docker_env: &mut HashMap<String, String>,
    toolchains: &[String],
    job_gh_host: Option<&str>,
) {
    if let Some(host_gh_config_dir) = resolve_gh_config_mount_source(
        toolchains,
        std::env::var_os("GH_CONFIG_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ) {
        args.push("-v".to_string());
        args.push(format!(
            "{}:{GH_CONFIG_CONTAINER_DIR}:ro",
            host_gh_config_dir.to_string_lossy()
        ));
        docker_env.insert(
            "GH_CONFIG_DIR".to_string(),
            GH_CONFIG_CONTAINER_DIR.to_string(),
        );
    }

    // Per-job repo host (when present and non-default) takes precedence over
    // the local-agent's process-wide GH_HOST. Without this, jobs whose repo
    // lives on a GHE host can't authenticate `gh` even when the host machine
    // has both auths configured.
    let host_gh_host = std::env::var_os("GH_HOST");
    let host_gh_host_str = host_gh_host
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let effective_host = job_gh_host
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .or(host_gh_host_str);

    if toolchains.iter().any(|t| t == "gh")
        && let Some(host) = effective_host
        && !host.eq_ignore_ascii_case(GH_DEFAULT_HOST)
        && !docker_env.contains_key("GH_HOST")
    {
        // Surface the host inside the container so `gh` commands target the
        // right forge by default. Skipped for github.com because gh's default
        // already matches and we want to avoid masking an unset env.
        docker_env.insert("GH_HOST".to_string(), host.to_string());
    }

    let effective_host_owned = effective_host.map(str::to_string);
    append_gh_auth_token_env_for_toolchains(
        docker_env,
        toolchains,
        effective_host_owned.as_deref().map(std::ffi::OsStr::new),
        resolve_gh_auth_token_from_host,
    );
}

fn append_gh_auth_token_env_for_toolchains<F>(
    docker_env: &mut HashMap<String, String>,
    toolchains: &[String],
    gh_host: Option<&std::ffi::OsStr>,
    resolve_token: F,
) where
    F: FnOnce(&str) -> Option<String>,
{
    if !toolchains.iter().any(|toolchain| toolchain == "gh") {
        return;
    }

    let host = gh_host
        .and_then(std::ffi::OsStr::to_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(GH_DEFAULT_HOST);

    let token_env_var = if host.eq_ignore_ascii_case(GH_DEFAULT_HOST) {
        "GH_TOKEN"
    } else {
        "GH_ENTERPRISE_TOKEN"
    };

    if docker_env.contains_key(token_env_var) {
        return;
    }

    if let Some(token) = resolve_token(host)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        docker_env.insert(token_env_var.to_string(), token);
    }
}

fn resolve_gh_auth_token_from_host(hostname: &str) -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token", "--hostname", hostname])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_gh_config_mount_source(
    toolchains: &[String],
    gh_config_dir: Option<&std::ffi::OsStr>,
    home_dir: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if !toolchains.iter().any(|toolchain| toolchain == "gh") {
        return None;
    }

    let candidate = if let Some(explicit) = gh_config_dir {
        PathBuf::from(explicit)
    } else {
        PathBuf::from(home_dir?).join(".config/gh")
    };

    if candidate.is_dir() {
        Some(candidate)
    } else {
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
        append_gh_auth_token_env_for_toolchains, append_gh_config_mount_for_toolchains,
        append_git_config_env, append_git_identity_env_defaults, docker_run_user_arg,
        is_restricted_breakout_mount, resolve_gh_config_mount_source,
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

    #[test]
    fn resolve_gh_config_mount_source_prefers_explicit_dir() {
        let temp_root =
            std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let explicit = temp_root.join("explicit-gh");
        let home_dir = temp_root.join("home");
        let fallback = home_dir.join(".config/gh");
        std::fs::create_dir_all(&explicit).expect("create explicit dir");
        std::fs::create_dir_all(&fallback).expect("create fallback dir");

        let resolved = resolve_gh_config_mount_source(
            &["gh".to_string()],
            Some(explicit.as_os_str()),
            Some(home_dir.as_os_str()),
        );

        assert_eq!(resolved, Some(explicit.clone()));
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn resolve_gh_config_mount_source_falls_back_to_home_config() {
        let temp_root =
            std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let home_dir = temp_root.join("home");
        let gh_dir = home_dir.join(".config/gh");
        std::fs::create_dir_all(&gh_dir).expect("create gh dir");

        let resolved =
            resolve_gh_config_mount_source(&["gh".to_string()], None, Some(home_dir.as_os_str()));

        assert_eq!(resolved, Some(gh_dir));
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn resolve_gh_config_mount_source_requires_gh_toolchain() {
        let temp_root =
            std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let explicit = temp_root.join("explicit-gh");
        std::fs::create_dir_all(&explicit).expect("create explicit dir");

        let resolved = resolve_gh_config_mount_source(
            &["git".to_string()],
            Some(explicit.as_os_str()),
            Some(temp_root.as_os_str()),
        );

        assert!(resolved.is_none());
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn append_gh_config_mount_for_toolchains_noops_without_gh_toolchain() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        append_gh_config_mount_for_toolchains(&mut args, &mut env, &["git".to_string()], None);

        assert!(args.is_empty());
        assert!(!env.contains_key("GH_CONFIG_DIR"));
    }

    #[test]
    fn append_gh_auth_token_env_uses_default_host_token() {
        let mut env = HashMap::new();
        append_gh_auth_token_env_for_toolchains(&mut env, &["gh".to_string()], None, |host| {
            assert_eq!(host, "github.com");
            Some("secret-token".to_string())
        });

        assert_eq!(
            env.get("GH_TOKEN").map(String::as_str),
            Some("secret-token")
        );
        assert!(!env.contains_key("GH_ENTERPRISE_TOKEN"));
    }

    #[test]
    fn append_gh_auth_token_env_uses_enterprise_token_for_non_default_host() {
        let mut env = HashMap::new();
        append_gh_auth_token_env_for_toolchains(
            &mut env,
            &["gh".to_string()],
            Some(std::ffi::OsStr::new("ghe.example.com")),
            |host| {
                assert_eq!(host, "ghe.example.com");
                Some("enterprise-token".to_string())
            },
        );

        assert_eq!(
            env.get("GH_ENTERPRISE_TOKEN").map(String::as_str),
            Some("enterprise-token")
        );
        assert!(!env.contains_key("GH_TOKEN"));
    }

    #[test]
    fn append_gh_auth_token_env_preserves_existing_token() {
        let mut env = HashMap::from([("GH_TOKEN".to_string(), "already-set".to_string())]);
        append_gh_auth_token_env_for_toolchains(&mut env, &["gh".to_string()], None, |_| {
            panic!("token resolver should not run when GH_TOKEN already exists")
        });

        assert_eq!(env.get("GH_TOKEN").map(String::as_str), Some("already-set"));
    }

    #[test]
    fn append_gh_auth_token_env_noops_without_gh_toolchain() {
        let mut env = HashMap::new();
        append_gh_auth_token_env_for_toolchains(&mut env, &["git".to_string()], None, |_| {
            panic!("token resolver should not run without gh toolchain")
        });

        assert!(env.is_empty());
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
        protocol::PlannedPermissionTier::RawCompatible => "raw_compatible",
        protocol::PlannedPermissionTier::KubernetesStub => "kubernetes_stub",
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
                network: None,
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
                network: None,
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
    fn resolve_runtime_spec_falls_back_and_keeps_repo_claims_from_context() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v1".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: None,
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
            repos: vec![protocol::PlannedRepoClaim {
                repo_url: "https://github.com/nickdavies/agent-hub-poc-test.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: true,
                target_branch: None,
                claim_type: "read_only".to_string(),
                branch_name: None,
            }],
        };
        let mut context = serde_json::json!({});
        protocol::project_runtime_spec_into_job_context(&mut context, &runtime);

        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context,
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

        let (resolved, source) = resolve_runtime_spec_and_source(&job);
        assert!(matches!(source, RuntimeSpecSource::LegacyJobFields));
        assert_eq!(resolved.repos.len(), 1);
        assert_eq!(
            resolved.repos[0].repo_url,
            "https://github.com/nickdavies/agent-hub-poc-test.git"
        );
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
            "local/opencode:managed".to_string(),
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

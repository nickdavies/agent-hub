use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use chrono::Utc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, watch};

pub mod credential_provider;
pub mod egress;
mod process;

pub use process::{
    ProcessIdentity, read_process_identity, terminate_owned_process_group,
    verify_process_group_absent,
};

#[derive(Debug, Clone)]
pub struct Deadline {
    monotonic: Option<Instant>,
    wall_clock: Option<SystemTime>,
}

impl Deadline {
    pub fn new(timeout_secs: Option<u64>) -> anyhow::Result<Self> {
        Self::from_start(Instant::now(), SystemTime::now(), timeout_secs)
    }

    pub fn from_start(
        monotonic_start: Instant,
        wall_clock_start: SystemTime,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<Self> {
        let timeout = timeout_secs.map(Duration::from_secs);
        Ok(Self {
            monotonic: timeout
                .map(|duration| {
                    monotonic_start
                        .checked_add(duration)
                        .context("execution monotonic deadline overflow")
                })
                .transpose()?,
            wall_clock: timeout
                .map(|duration| {
                    wall_clock_start
                        .checked_add(duration)
                        .context("execution wall-clock deadline overflow")
                })
                .transpose()?,
        })
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    pub fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.monotonic
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub fn is_expired(&self) -> bool {
        self.remaining()
            .is_some_and(|remaining| remaining.is_zero())
    }

    pub fn wall_clock(&self) -> Option<SystemTime> {
        self.wall_clock
    }

    pub fn instant(&self) -> Option<Instant> {
        self.monotonic
    }
}

#[derive(Debug, Clone)]
pub struct Cancellation {
    receiver: watch::Receiver<bool>,
}

impl Cancellation {
    pub fn new(receiver: watch::Receiver<bool>) -> Self {
        Self { receiver }
    }

    pub fn requested(&self) -> bool {
        *self.receiver.borrow()
    }

    pub fn never() -> Self {
        let (_sender, receiver) = watch::channel(false);
        Self::new(receiver)
    }

    async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ExternalEffectRecord {
    operation_id: uuid::Uuid,
    operation_kind: String,
    resource_identity: String,
    status: ExternalEffectStatus,
    recorded_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExternalEffectStatus {
    Submitting,
    Submitted,
    Ambiguous,
    CleanupFailed,
    Reconciled,
}

#[derive(Debug, Clone)]
pub struct ExecutionCancellationContext {
    cancellation: Cancellation,
    journal_path: Arc<PathBuf>,
    journal_lock: Arc<std::sync::Mutex<()>>,
    effect_state: Arc<std::sync::Mutex<HashMap<uuid::Uuid, ExternalEffectRecord>>>,
}

impl ExecutionCancellationContext {
    pub fn new(root: &Path, cancellation: Cancellation) -> anyhow::Result<Self> {
        ensure_private_directory(root)?;
        let journal_path = root.join("executor-effects.jsonl");
        let mut effect_state = HashMap::new();
        if journal_path.exists() {
            let bytes = std::fs::read(&journal_path)?;
            anyhow::ensure!(
                bytes.is_empty() || bytes.last() == Some(&b'\n'),
                "external effect journal is truncated"
            );
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let record: ExternalEffectRecord = serde_json::from_slice(line)?;
                effect_state.insert(record.operation_id, record);
            }
        }
        Ok(Self {
            cancellation,
            journal_path: Arc::new(journal_path),
            journal_lock: Arc::new(std::sync::Mutex::new(())),
            effect_state: Arc::new(std::sync::Mutex::new(effect_state)),
        })
    }

    pub fn non_cancellable_recovery(&self) -> Self {
        Self {
            cancellation: Cancellation::never(),
            journal_path: Arc::clone(&self.journal_path),
            journal_lock: Arc::clone(&self.journal_lock),
            effect_state: Arc::clone(&self.effect_state),
        }
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.requested(), "execution was cancelled");
        Ok(())
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub fn finalize(&self) -> anyhow::Result<()> {
        let state = self
            .effect_state
            .lock()
            .expect("effect state lock poisoned");
        anyhow::ensure!(
            state.values().all(effect_is_resolved),
            "external effects remain unresolved"
        );
        drop(state);
        sync_directory(
            self.journal_path
                .parent()
                .context("effect journal has no parent")?,
        )
    }

    pub fn reconcile_operation_kind_resource(
        &self,
        operation_kind: &str,
        resource_identity: &str,
    ) -> anyhow::Result<()> {
        let unresolved = self
            .effect_state
            .lock()
            .expect("effect state lock poisoned")
            .values()
            .filter(|record| {
                record.operation_kind == operation_kind
                    && (record.resource_identity == resource_identity
                        || record
                            .resource_identity
                            .split(';')
                            .any(|field| field == format!("container={resource_identity}")))
                    && !effect_is_resolved(record)
            })
            .cloned()
            .collect::<Vec<_>>();
        for record in unresolved {
            self.append_effect(
                record.operation_id,
                &record.operation_kind,
                &record.resource_identity,
                ExternalEffectStatus::Reconciled,
            )?;
        }
        Ok(())
    }

    pub fn required_resource_quiescence(
        &self,
        resource_identity: &str,
        normal: Duration,
        ambiguous: Duration,
    ) -> Duration {
        let ambiguous_submission = self
            .effect_state
            .lock()
            .expect("effect state lock poisoned")
            .values()
            .any(|record| {
                operation_requires_recovery(&record.operation_kind)
                    && matches!(
                        record.status,
                        ExternalEffectStatus::Submitting | ExternalEffectStatus::Ambiguous
                    )
                    && (record.resource_identity == resource_identity
                        || record
                            .resource_identity
                            .split(';')
                            .any(|field| field.ends_with(&format!("={resource_identity}"))))
            });
        if ambiguous_submission {
            normal.max(ambiguous)
        } else {
            normal
        }
    }

    pub async fn reconcile_owned_effects(&self, credential_root: &Path) -> anyhow::Result<()> {
        let records = self
            .effect_state
            .lock()
            .expect("effect state lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for record in records {
            if effect_is_resolved(&record) {
                continue;
            }
            let recovered = match record.operation_kind.as_str() {
                "credential_materialize" | "credential_secret_write" => {
                    let path = Path::new(&record.resource_identity);
                    anyhow::ensure!(
                        path.is_absolute() && path.starts_with(credential_root),
                        "credential effect path escaped its owned root"
                    );
                    match std::fs::remove_file(path) {
                        Ok(()) => sync_directory(
                            path.parent()
                                .context("credential effect path has no parent")?,
                        )?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    anyhow::ensure!(
                        !path.exists(),
                        "owned credential file remains after cleanup"
                    );
                    true
                }
                "credential_directory_create" => {
                    let path = Path::new(&record.resource_identity);
                    anyhow::ensure!(
                        path.is_absolute() && path.starts_with(credential_root),
                        "credential effect directory escaped its owned root"
                    );
                    if path.is_dir() && std::fs::read_dir(path)?.next().is_none() {
                        std::fs::remove_dir(path)?;
                        sync_directory(
                            path.parent()
                                .context("credential effect directory has no parent")?,
                        )?;
                    }
                    anyhow::ensure!(
                        !path.exists(),
                        "owned credential directory remains after cleanup"
                    );
                    true
                }
                "credential_helper_process" => {
                    verify_process_group_absent(&parse_process_effect_identity(
                        &record.resource_identity,
                    )?)
                    .await?;
                    true
                }
                _ => false,
            };
            if recovered {
                self.append_effect(
                    record.operation_id,
                    &record.operation_kind,
                    &record.resource_identity,
                    ExternalEffectStatus::Reconciled,
                )?;
            }
        }
        Ok(())
    }

    pub async fn settle_process_helpers(&self) -> anyhow::Result<()> {
        let records = self
            .effect_state
            .lock()
            .expect("effect state lock poisoned")
            .values()
            .filter(|record| {
                ExecutorHelperOperationKind::try_from(record.operation_kind.as_str()).is_ok()
                    && !effect_is_resolved(record)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for record in records {
            let result = async {
                let identity = parse_process_effect_identity(&record.resource_identity)?;
                terminate_owned_process_group(&identity).await?;
                self.append_effect(
                    record.operation_id,
                    &record.operation_kind,
                    &record.resource_identity,
                    ExternalEffectStatus::Reconciled,
                )
            }
            .await;
            if let Err(error) = result {
                failures.push(format!(
                    "{} {}: {error:#}",
                    record.operation_kind, record.operation_id
                ));
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "helper settlement failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    pub fn begin_external_operation<'a>(
        &'a self,
        operation_kind: &str,
        resource_identity: &str,
    ) -> anyhow::Result<ExternalOperation<'a>> {
        anyhow::ensure!(
            !operation_kind.trim().is_empty(),
            "external operation kind is empty"
        );
        anyhow::ensure!(
            !resource_identity.chars().any(char::is_control),
            "external operation resource contains control characters"
        );
        let operation_id = uuid::Uuid::new_v4();
        self.append_effect(
            operation_id,
            operation_kind,
            resource_identity,
            ExternalEffectStatus::Submitting,
        )?;
        Ok(ExternalOperation {
            context: self,
            operation_id,
            operation_kind: operation_kind.to_string(),
            resource_identity: resource_identity.to_string(),
            resolved: false,
        })
    }

    fn append_effect(
        &self,
        operation_id: uuid::Uuid,
        operation_kind: &str,
        resource_identity: &str,
        status: ExternalEffectStatus,
    ) -> anyhow::Result<()> {
        use std::io::Write as _;
        let _guard = self
            .journal_lock
            .lock()
            .expect("effect journal lock poisoned");
        let record = ExternalEffectRecord {
            operation_id,
            operation_kind: operation_kind.to_string(),
            resource_identity: resource_identity.to_string(),
            status,
            recorded_at: Utc::now(),
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(self.journal_path.as_ref())?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if let Some(parent) = self.journal_path.parent() {
            sync_directory(parent)?;
        }
        self.effect_state
            .lock()
            .expect("effect state lock poisoned")
            .insert(operation_id, record);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path, receiver: watch::Receiver<bool>) -> anyhow::Result<Self> {
        Self::new(root, Cancellation::new(receiver))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorHelperOperationKind {
    DockerMutation,
    JournalledSubprocess,
    CredentialLegacy,
}

impl TryFrom<&str> for ExecutorHelperOperationKind {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "docker_mutation_helper_process" => Ok(Self::DockerMutation),
            "journalled_subprocess_helper_process" => Ok(Self::JournalledSubprocess),
            "credential_helper_process" => Ok(Self::CredentialLegacy),
            _ => Err(()),
        }
    }
}

fn parse_process_effect_identity(value: &str) -> anyhow::Result<ProcessIdentity> {
    if value.trim_start().starts_with('{') {
        #[derive(serde::Deserialize)]
        struct LinkedProcess {
            parent_operation_id: uuid::Uuid,
            process: ProcessIdentity,
        }
        let linked: LinkedProcess = serde_json::from_str(value)?;
        let _ = linked.parent_operation_id;
        return Ok(linked.process);
    }
    let fields = value
        .split(';')
        .filter_map(|field| field.split_once('='))
        .collect::<HashMap<_, _>>();
    Ok(ProcessIdentity {
        pid: fields
            .get("pid")
            .context("process effect has no PID")?
            .parse()?,
        pgid: fields
            .get("pgid")
            .context("process effect has no process group")?
            .parse()?,
        linux_boot_id: fields
            .get("boot")
            .context("process effect has no boot identity")?
            .to_string(),
        proc_start_time_ticks: fields
            .get("start")
            .context("process effect has no start identity")?
            .parse()?,
    })
}

fn format_process_effect_identity(identity: &ProcessIdentity) -> String {
    format!(
        "pid={};pgid={};boot={};start={}",
        identity.pid, identity.pgid, identity.linux_boot_id, identity.proc_start_time_ticks
    )
}

fn effect_is_resolved(record: &ExternalEffectRecord) -> bool {
    record.status == ExternalEffectStatus::Reconciled
}

fn operation_requires_recovery(operation_kind: &str) -> bool {
    matches!(
        operation_kind,
        "primary_docker_mutation"
            | "egress_network_create_request"
            | "egress_resolver_create_request"
            | "egress_proxy_create_request"
            | "egress_network_connect_request"
    )
}

pub struct ExternalOperation<'a> {
    context: &'a ExecutionCancellationContext,
    operation_id: uuid::Uuid,
    operation_kind: String,
    resource_identity: String,
    resolved: bool,
}

impl ExternalOperation<'_> {
    pub fn completed(mut self) -> anyhow::Result<()> {
        self.finish(ExternalEffectStatus::Submitted)
    }

    pub fn ambiguous(mut self, _error_category: &str) -> anyhow::Result<()> {
        self.finish(ExternalEffectStatus::Ambiguous)
    }

    pub fn cleanup_failed(mut self, _error_category: &str) -> anyhow::Result<()> {
        self.finish(ExternalEffectStatus::CleanupFailed)
    }

    pub fn reconciled(mut self) -> anyhow::Result<()> {
        self.finish(ExternalEffectStatus::Reconciled)
    }

    fn finish(&mut self, status: ExternalEffectStatus) -> anyhow::Result<()> {
        self.context.append_effect(
            self.operation_id,
            &self.operation_kind,
            &self.resource_identity,
            status,
        )?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for ExternalOperation<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            let _ = self.context.append_effect(
                self.operation_id,
                &self.operation_kind,
                &self.resource_identity,
                ExternalEffectStatus::Ambiguous,
            );
        }
    }
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "effect journal root is not a directory"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        anyhow::ensure!(
            metadata.uid() == unsafe { geteuid() },
            "effect journal root owner mismatch"
        );
    }
    Ok(())
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPaths {
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub transcript: PathBuf,
}

impl ArtifactPaths {
    pub fn in_directory(directory: &Path) -> Self {
        Self {
            stdout: directory.join("stdout.log"),
            stderr: directory.join("stderr.log"),
            transcript: directory.join("transcript.jsonl"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub artifacts: ArtifactPaths,
    pub deadline: Deadline,
    pub cancellation: Cancellation,
    pub append: bool,
    pub redaction_values: Vec<String>,
    pub output_limits: OutputLimits,
    pub effects: Option<ExecutionCancellationContext>,
    pub operation_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimits {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub transcript_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Exited(i32),
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentStatus {
    VerifiedAbsent,
    NotApplicable,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentProof {
    pub process: ContainmentStatus,
    pub container: ContainmentStatus,
    pub egress: ContainmentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupStatus {
    Complete,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub status: ExecutionStatus,
    pub containment: ContainmentProof,
    pub cleanup: CleanupStatus,
}

pub async fn execute_process(request: ExecutionRequest) -> anyhow::Result<ExecutionResult> {
    anyhow::ensure!(
        !request.cancellation.requested(),
        "execution was cancelled before process creation"
    );
    let stdout_file = open_artifact(&request.artifacts.stdout, request.append)?;
    let stderr_file = open_artifact(&request.artifacts.stderr, request.append)?;
    let transcript_file = open_artifact(&request.artifacts.transcript, request.append)?;
    let transcript = Arc::new(Mutex::new(BoundedTranscript {
        writer: tokio::io::BufWriter::new(tokio::fs::File::from_std(transcript_file)),
        written: if request.append {
            std::fs::metadata(&request.artifacts.transcript)?.len()
        } else {
            0
        },
        limit: request.output_limits.transcript_bytes,
    }));
    let (stream_failure_tx, stream_failure_rx) = watch::channel(None::<String>);

    let mut command = tokio::process::Command::new(&request.command);
    command
        .env_clear()
        .args(&request.args)
        .current_dir(&request.current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in &request.environment {
        command.env(name, value);
    }
    #[cfg(unix)]
    command.as_std_mut().process_group(0);

    let submission = match (&request.effects, &request.operation_identity) {
        (Some(effects), Some(identity)) => {
            Some(effects.begin_external_operation("primary_docker_mutation", identity)?)
        }
        (None, None) => None,
        _ => anyhow::bail!(
            "execution effect context and operation identity must be provided together"
        ),
    };
    let mut child = command.spawn().with_context(|| {
        format!(
            "spawn resolved command '{}'",
            request.command.to_string_lossy()
        )
    })?;
    let process_identity = read_process_identity(
        child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .context("spawned process has no valid process ID")?,
    )?;
    let helper = request
        .effects
        .as_ref()
        .map(|effects| {
            effects.begin_external_operation(
                "docker_mutation_helper_process",
                &format_process_effect_identity(&process_identity),
            )
        })
        .transpose()?;
    let stdout = child
        .stdout
        .take()
        .context("capture resolved command stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capture resolved command stderr")?;
    let stdout_redactions = request.redaction_values.clone();
    let stderr_redactions = request.redaction_values.clone();
    let stdout_limit = request.output_limits.stdout_bytes;
    let stderr_limit = request.output_limits.stderr_bytes;
    let stdout_transcript = Arc::clone(&transcript);
    let stdout_failure = stream_failure_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let result = persist_stream(
            stdout,
            tokio::fs::File::from_std(stdout_file),
            stdout_transcript,
            "stdout",
            stdout_redactions,
            stdout_limit,
            stdout_failure.clone(),
        )
        .await;
        if let Err(error) = &result {
            let _ = stdout_failure.send(Some(format!("stdout persistence failed: {error:#}")));
        }
        result
    });
    let stderr_failure = stream_failure_tx;
    let stderr_task = tokio::spawn(async move {
        let result = persist_stream(
            stderr,
            tokio::fs::File::from_std(stderr_file),
            transcript,
            "stderr",
            stderr_redactions,
            stderr_limit,
            stderr_failure.clone(),
        )
        .await;
        if let Err(error) = &result {
            let _ = stderr_failure.send(Some(format!("stderr persistence failed: {error:#}")));
        }
        result
    });

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let status = loop {
        tokio::select! {
            waited = child.wait() => {
                let status = waited.context("wait for resolved command")?;
                break ExecutionStatus::Exited(status.code().unwrap_or(1));
            }
            _ = ticker.tick() => {
                if stream_failure_rx.borrow().is_some() {
                    terminate_process_group(&process_identity, &mut child).await?;
                    break ExecutionStatus::OutputLimitExceeded;
                }
                if request.cancellation.requested() {
                    terminate_process_group(&process_identity, &mut child).await?;
                    break ExecutionStatus::Cancelled;
                }
                if request.deadline.is_expired() {
                    terminate_process_group(&process_identity, &mut child).await?;
                    break ExecutionStatus::TimedOut;
                }
            }
        }
    };
    verify_process_group_absent(&process_identity).await?;
    if let Some(helper) = helper {
        helper.reconciled()?;
    }
    if let Some(submission) = submission {
        match status {
            ExecutionStatus::Exited(_) => submission.completed()?,
            ExecutionStatus::Cancelled
            | ExecutionStatus::TimedOut
            | ExecutionStatus::OutputLimitExceeded => {
                submission.ambiguous("primary_docker_mutation_outcome_unknown")?
            }
        }
    }
    stdout_task
        .await
        .context("stdout persistence task failed")??;
    stderr_task
        .await
        .context("stderr persistence task failed")??;
    let status = if stream_failure_rx.borrow().is_some() {
        ExecutionStatus::OutputLimitExceeded
    } else {
        status
    };
    Ok(ExecutionResult {
        status,
        containment: ContainmentProof {
            process: ContainmentStatus::VerifiedAbsent,
            container: ContainmentStatus::NotApplicable,
            egress: ContainmentStatus::NotApplicable,
        },
        cleanup: CleanupStatus::Complete,
    })
}

fn open_artifact(path: &Path, append: bool) -> anyhow::Result<std::fs::File> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "execution artifact path is empty"
    );
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(path)
        .with_context(|| format!("open execution artifact {}", path.display()))
}

async fn persist_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    output: tokio::fs::File,
    transcript: Arc<Mutex<BoundedTranscript>>,
    stream: &'static str,
    redaction_values: Vec<String>,
    limit: u64,
    failure: watch::Sender<Option<String>>,
) -> anyhow::Result<()> {
    const CHUNK_BYTES: usize = 8 * 1024;
    let mut output = tokio::io::BufWriter::new(output);
    let mut written = output.get_ref().metadata().await?.len();
    anyhow::ensure!(
        written <= limit,
        "existing {stream} output exceeds reviewed limit"
    );
    let redactions = redaction_values
        .iter()
        .filter(|value| !value.is_empty())
        .map(|value| value.as_bytes())
        .collect::<Vec<_>>();
    let retained = redactions
        .iter()
        .map(|value| value.len().saturating_sub(1))
        .max()
        .unwrap_or(0);
    let mut pending = Vec::new();
    let mut utf8_pending = Vec::new();
    let mut buffer = [0_u8; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        let eof = read == 0;
        if read != 0 {
            anyhow::ensure!(
                pending.len() <= retained,
                "stream redaction carry exceeded its bound"
            );
            pending.extend_from_slice(&buffer[..read]);
        }
        let emit_len = if eof {
            pending.len()
        } else {
            safe_redaction_prefix(
                &pending,
                pending.len().saturating_sub(retained),
                &redactions,
            )
        };
        if emit_len == 0 && !eof {
            continue;
        }
        let Some(redacted) =
            redact_bounded_bytes(&pending[..emit_len], &redactions, limit - written)?
        else {
            let message = format!("{stream} output exceeded reviewed limit of {limit} bytes");
            let _ = failure.send(Some(message));
            return Ok(());
        };
        if written.saturating_add(redacted.len() as u64) > limit {
            let message = format!("{stream} output exceeded reviewed limit of {limit} bytes");
            let _ = failure.send(Some(message));
            return Ok(());
        }
        if !redacted.is_empty() {
            output.write_all(&redacted).await?;
            output.flush().await?;
            written += redacted.len() as u64;
            persist_transcript_chunks(
                &redacted,
                &mut utf8_pending,
                Arc::clone(&transcript),
                stream,
                &failure,
            )
            .await?;
        }
        pending.drain(..emit_len);
        if eof {
            anyhow::ensure!(
                utf8_pending.is_empty(),
                "{stream} ended with incomplete UTF-8"
            );
            break;
        }
    }
    Ok(())
}

fn safe_redaction_prefix(input: &[u8], nominal: usize, redactions: &[&[u8]]) -> usize {
    let earliest = nominal.saturating_sub(
        redactions
            .iter()
            .map(|value| value.len().saturating_sub(1))
            .max()
            .unwrap_or(0),
    );
    (earliest..nominal)
        .find(|offset| {
            redactions.iter().any(|value| {
                input[*offset..].starts_with(value) && offset.saturating_add(value.len()) > nominal
            })
        })
        .unwrap_or(nominal)
}

fn redact_bounded_bytes(
    input: &[u8],
    redactions: &[&[u8]],
    remaining: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    const REPLACEMENT: &[u8] = b"[REDACTED]";
    let mut required = 0_u64;
    let mut offset = 0;
    while offset < input.len() {
        if let Some(value) = redactions
            .iter()
            .find(|value| input[offset..].starts_with(value))
        {
            required = required.saturating_add(REPLACEMENT.len() as u64);
            offset += value.len();
        } else {
            required = required.saturating_add(1);
            offset += 1;
        }
        if required > remaining {
            return Ok(None);
        }
    }
    let mut output = Vec::with_capacity(usize::try_from(required)?);
    let mut offset = 0;
    while offset < input.len() {
        if let Some(value) = redactions
            .iter()
            .find(|value| input[offset..].starts_with(value))
        {
            output.extend_from_slice(REPLACEMENT);
            offset += value.len();
        } else {
            output.push(input[offset]);
            offset += 1;
        }
    }
    Ok(Some(output))
}

async fn persist_transcript_chunks(
    bytes: &[u8],
    utf8_pending: &mut Vec<u8>,
    transcript: Arc<Mutex<BoundedTranscript>>,
    stream: &'static str,
    failure: &watch::Sender<Option<String>>,
) -> anyhow::Result<()> {
    utf8_pending.extend_from_slice(bytes);
    let (valid_len, incomplete) = match std::str::from_utf8(utf8_pending) {
        Ok(_) => (utf8_pending.len(), false),
        Err(error) if error.error_len().is_none() => (error.valid_up_to(), true),
        Err(error) => anyhow::bail!(
            "{stream} output is not UTF-8 at byte {}",
            error.valid_up_to()
        ),
    };
    if valid_len == 0 {
        anyhow::ensure!(incomplete && utf8_pending.len() <= 3, "invalid UTF-8 carry");
        return Ok(());
    }
    let valid = std::str::from_utf8(&utf8_pending[..valid_len])?;
    let mut consumed = 0;
    for segment in valid.split_inclusive('\n') {
        let complete = segment.ends_with('\n');
        let text = segment.strip_suffix('\n').unwrap_or(segment);
        write_transcript_record(Arc::clone(&transcript), stream, text, !complete, failure).await?;
        consumed += segment.len();
    }
    utf8_pending.drain(..consumed);
    if incomplete {
        anyhow::ensure!(utf8_pending.len() <= 3, "UTF-8 carry exceeded three bytes");
    }
    Ok(())
}

async fn write_transcript_record(
    transcript: Arc<Mutex<BoundedTranscript>>,
    stream: &'static str,
    text: &str,
    partial: bool,
    failure: &watch::Sender<Option<String>>,
) -> anyhow::Result<()> {
    let mut transcript = transcript.lock().await;
    let remaining = transcript.limit.saturating_sub(transcript.written);
    let timestamp = Utc::now().to_rfc3339();
    let record = TranscriptRecord {
        timestamp: &timestamp,
        stream,
        line: text,
        partial,
    };
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, &record)?;
    let required = counter.written.saturating_add(1);
    if required > remaining {
        let message = format!(
            "transcript output exceeded reviewed limit of {} bytes",
            transcript.limit
        );
        let _ = failure.send(Some(message));
        return Ok(());
    }
    let mut encoded = Vec::with_capacity(usize::try_from(required)?);
    serde_json::to_writer(&mut encoded, &record)?;
    encoded.push(b'\n');
    transcript.writer.write_all(&encoded).await?;
    transcript.writer.flush().await?;
    transcript.written += required;
    Ok(())
}

#[derive(serde::Serialize)]
struct TranscriptRecord<'a> {
    timestamp: &'a str,
    stream: &'a str,
    line: &'a str,
    partial: bool,
}

#[derive(Default)]
struct CountingWriter {
    written: u64,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BoundedTranscript {
    writer: tokio::io::BufWriter<tokio::fs::File>,
    written: u64,
    limit: u64,
}

async fn terminate_process_group(
    identity: &ProcessIdentity,
    child: &mut tokio::process::Child,
) -> anyhow::Result<()> {
    terminate_owned_process_group(identity).await?;
    let _ = child.wait().await;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerMount {
    pub host_path: PathBuf,
    pub container_path: String,
    pub read_only: bool,
}

impl DockerMount {
    pub fn new(
        host_path: impl Into<PathBuf>,
        container_path: impl Into<String>,
        read_only: bool,
    ) -> anyhow::Result<Self> {
        let mount = Self {
            host_path: host_path.into(),
            container_path: container_path.into(),
            read_only,
        };
        validate_container_path(&mount.container_path)?;
        anyhow::ensure!(
            !mount.host_path.as_os_str().is_empty(),
            "Docker mount source is empty"
        );
        anyhow::ensure!(
            mount.host_path.is_absolute(),
            "Docker mount source must be absolute"
        );
        let host = mount.host_path.to_string_lossy();
        anyhow::ensure!(
            !host.contains(',') && !host.chars().any(char::is_control),
            "Docker mount source cannot be represented safely"
        );
        Ok(mount)
    }

    pub fn argument(&self) -> String {
        format!(
            "{}:{}{}",
            self.host_path.to_string_lossy(),
            self.container_path,
            if self.read_only { ":ro" } else { "" }
        )
    }

    pub fn bind_argument(&self) -> String {
        format!(
            "type=bind,src={},dst={}{}",
            self.host_path.to_string_lossy(),
            self.container_path,
            if self.read_only { ",readonly" } else { "" }
        )
    }
}

fn validate_container_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.starts_with('/'),
        "Docker mount destination must be absolute"
    );
    anyhow::ensure!(
        !path.contains(':')
            && !path.contains(',')
            && !path.contains("//")
            && !path.chars().any(char::is_control),
        "Docker mount destination is not canonical"
    );
    anyhow::ensure!(
        Path::new(path)
            .components()
            .all(|component| !matches!(component, Component::ParentDir | Component::CurDir)),
        "Docker mount destination is not canonical"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DockerOwnership {
    labels: BTreeMap<String, String>,
}

impl DockerOwnership {
    pub fn new(labels: BTreeMap<String, String>) -> anyhow::Result<Self> {
        anyhow::ensure!(!labels.is_empty(), "Docker ownership labels are empty");
        anyhow::ensure!(
            labels
                .iter()
                .all(|(name, value)| !name.is_empty() && !value.is_empty()),
            "Docker ownership label names and values must be non-empty"
        );
        Ok(Self { labels })
    }

    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    pub fn label_args(&self) -> Vec<String> {
        self.labels
            .iter()
            .flat_map(|(name, value)| ["--label".to_string(), format!("{name}={value}")])
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct DockerCli {
    command: PathBuf,
    effects: Option<ExecutionCancellationContext>,
    absence_quiescence: Duration,
    ambiguous_quiescence: Duration,
}

impl DockerCli {
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            effects: None,
            absence_quiescence: Duration::from_secs(2),
            ambiguous_quiescence: Duration::from_secs(30),
        }
    }

    pub fn with_effects(mut self, effects: ExecutionCancellationContext) -> Self {
        self.effects = Some(effects);
        self
    }

    pub fn with_absence_quiescence(mut self, duration: Duration) -> Self {
        self.absence_quiescence = duration;
        self
    }

    pub fn with_ambiguous_quiescence(mut self, duration: Duration) -> Self {
        self.ambiguous_quiescence = duration;
        self
    }

    pub async fn inspect_ownership(
        &self,
        name: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<BTreeMap<String, String>>> {
        let output = self
            .run_journalled(
                &[
                    "container".to_string(),
                    "inspect".to_string(),
                    "--format".to_string(),
                    "{{json .Config.Labels}}".to_string(),
                    name.to_string(),
                ],
                timeout,
                "docker_container_inspect",
                name,
            )
            .await?;
        if output.status.success() {
            return serde_json::from_slice(&output.stdout)
                .context("Docker ownership labels are not valid JSON")
                .map(Some);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("no such container") || stderr.contains("no such object") {
            Ok(None)
        } else {
            anyhow::bail!(
                "docker container inspect {name} failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
    }

    pub async fn remove_owned(
        &self,
        name: &str,
        expected: &DockerOwnership,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let required_quiescence =
            self.effects
                .as_ref()
                .map_or(self.absence_quiescence, |effects| {
                    effects.required_resource_quiescence(
                        name,
                        self.absence_quiescence,
                        self.ambiguous_quiescence,
                    )
                });
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("container cleanup deadline overflow")?;
        let mut absent_since = None;
        loop {
            let now = Instant::now();
            let actual = self
                .inspect_ownership(name, remaining(deadline)?.min(Duration::from_secs(10)))
                .await?;
            match actual {
                None => {
                    let first = absent_since.get_or_insert(now);
                    if now.duration_since(*first) >= required_quiescence {
                        if let Some(effects) = &self.effects {
                            effects.reconcile_operation_kind_resource(
                                "primary_docker_mutation",
                                name,
                            )?;
                        }
                        return Ok(());
                    }
                    let needed = required_quiescence.saturating_sub(now.duration_since(*first));
                    anyhow::ensure!(
                        remaining(deadline)? >= needed,
                        "could not prove container '{name}' absent through continuous quiescence"
                    );
                    tokio::time::sleep(Duration::from_millis(50).min(needed)).await;
                }
                Some(actual) => {
                    absent_since = None;
                    ensure_expected_labels(name, expected, &actual)?;
                    let output = self
                        .run_journalled(
                            &["rm".to_string(), "-f".to_string(), name.to_string()],
                            remaining(deadline)?.min(Duration::from_secs(10)),
                            "docker_container_remove",
                            name,
                        )
                        .await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "docker rm -f {name} failed with {}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
            }
        }
    }

    async fn run_journalled(
        &self,
        args: &[String],
        timeout: Duration,
        operation_kind: &str,
        resource_identity: &str,
    ) -> anyhow::Result<std::process::Output> {
        anyhow::ensure!(!timeout.is_zero(), "Docker operation deadline exceeded");
        let operation = self
            .effects
            .as_ref()
            .map(|effects| effects.begin_external_operation(operation_kind, resource_identity))
            .transpose()?;
        let spawned = spawn_bounded_output(&self.command, args, "journalled Docker helper", true)?;
        let helper_identity = format_process_effect_identity(&spawned.identity);
        let helper = self
            .effects
            .as_ref()
            .map(|effects| {
                effects.begin_external_operation("docker_mutation_helper_process", &helper_identity)
            })
            .transpose()?;
        let output = match settle_bounded_output(
            spawned,
            timeout,
            &Cancellation::never(),
            "journalled Docker helper",
        )
        .await
        {
            Ok(output) => output,
            Err(failure) => {
                if let Some(helper) = helper {
                    if failure.helper_settled {
                        helper.reconciled()?;
                    } else {
                        helper.ambiguous("docker_helper_settlement_uncertain")?;
                    }
                }
                if let Some(operation) = operation {
                    operation.ambiguous("docker_operation_outcome_unknown")?;
                }
                return Err(failure.error);
            }
        };
        if let Some(helper) = helper {
            helper.reconciled()?;
        }
        if let Some(operation) = operation {
            operation.reconciled()?;
        }
        if let Some(effects) = &self.effects {
            effects.reconcile_operation_kind_resource(operation_kind, resource_identity)?;
        }
        Ok(output)
    }
}

fn ensure_expected_labels(
    name: &str,
    expected: &DockerOwnership,
    actual: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual == expected.labels(),
        "refusing to remove container '{name}' with mismatched ownership"
    );
    Ok(())
}

fn remaining(deadline: Instant) -> anyhow::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!remaining.is_zero(), "container cleanup deadline exceeded");
    Ok(remaining)
}

pub async fn inspect_image_identity(
    docker_command: &Path,
    configured_image: &str,
    deadline: &Deadline,
    cancellation: &Cancellation,
) -> anyhow::Result<String> {
    let remaining = deadline
        .remaining()
        .context("image resolution requires an absolute deadline")?;
    anyhow::ensure!(!remaining.is_zero(), "image resolution deadline exceeded");
    let output = execute_output_cancellable(
        docker_command,
        &[
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Id}}".to_string(),
            configured_image.to_string(),
        ],
        deadline,
        cancellation,
        "image resolution",
    )
    .await?;
    anyhow::ensure!(
        output.status.success(),
        "failed to resolve local image '{}': {}",
        configured_image,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let identity = String::from_utf8(output.stdout)
        .context("resolved image identity is not UTF-8")?
        .trim()
        .to_string();
    validate_image_identity(&identity)?;
    Ok(identity)
}

pub async fn inspect_image_identity_journalled(
    docker_command: &Path,
    configured_image: &str,
    deadline: &Deadline,
    effects: &ExecutionCancellationContext,
    resource_identity: &str,
) -> anyhow::Result<String> {
    let output = execute_output_journalled(
        docker_command,
        &[
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Id}}".to_string(),
            configured_image.to_string(),
        ],
        deadline,
        effects,
        "egress_docker_read",
        resource_identity,
        JournalledEffect::ReadOnly,
        "resolve local image",
    )
    .await?;
    anyhow::ensure!(
        output.status.success(),
        "failed to resolve local image '{}': {}",
        configured_image,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let identity = String::from_utf8(output.stdout)
        .context("resolved image identity is not UTF-8")?
        .trim()
        .to_string();
    validate_image_identity(&identity)?;
    Ok(identity)
}

pub async fn execute_output(
    command: &Path,
    args: &[String],
    deadline: &Deadline,
    description: &str,
) -> anyhow::Result<std::process::Output> {
    execute_output_cancellable(command, args, deadline, &Cancellation::never(), description).await
}

pub async fn execute_output_cancellable(
    command: &Path,
    args: &[String],
    deadline: &Deadline,
    cancellation: &Cancellation,
    description: &str,
) -> anyhow::Result<std::process::Output> {
    let remaining = deadline
        .remaining()
        .with_context(|| format!("{description} requires an absolute deadline"))?;
    anyhow::ensure!(!remaining.is_zero(), "{description} deadline exceeded");
    anyhow::ensure!(!cancellation.requested(), "{description} was cancelled");
    let spawned = spawn_bounded_output(command, args, description, true)?;
    settle_bounded_output(spawned, remaining, cancellation, description)
        .await
        .map_err(|failure| failure.error)
}

const SUBPROCESS_STREAM_LIMIT: u64 = 1024 * 1024;
const SUBPROCESS_AGGREGATE_LIMIT: u64 = 2 * SUBPROCESS_STREAM_LIMIT;

struct SpawnedBoundedOutput {
    child: tokio::process::Child,
    identity: ProcessIdentity,
    stdout: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    stderr: tokio::task::JoinHandle<anyhow::Result<Vec<u8>>>,
    failure: watch::Receiver<Option<String>>,
}

fn spawn_bounded_output(
    command: &Path,
    args: &[String],
    description: &str,
    clear_environment: bool,
) -> anyhow::Result<SpawnedBoundedOutput> {
    let mut process = tokio::process::Command::new(command);
    if clear_environment {
        process.env_clear();
    }
    process
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.as_std_mut().process_group(0);
    let mut child = process
        .spawn()
        .with_context(|| format!("spawn {description}"))?;
    let identity = read_process_identity(
        child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .context("bounded subprocess has no valid PID")?,
    )?;
    let stdout = child.stdout.take().context("capture bounded stdout")?;
    let stderr = child.stderr.take().context("capture bounded stderr")?;
    let aggregate = Arc::new(Mutex::new(0_u64));
    let (failure_tx, failure) = watch::channel(None::<String>);
    let stdout_aggregate = Arc::clone(&aggregate);
    let stdout_failure = failure_tx.clone();
    let stdout = tokio::spawn(async move {
        read_bounded_output_stream(stdout, stdout_aggregate, "stdout", stdout_failure).await
    });
    let stderr = tokio::spawn(async move {
        read_bounded_output_stream(stderr, aggregate, "stderr", failure_tx).await
    });
    Ok(SpawnedBoundedOutput {
        child,
        identity,
        stdout,
        stderr,
        failure,
    })
}

async fn read_bounded_output_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    aggregate: Arc<Mutex<u64>>,
    stream: &'static str,
    failure: watch::Sender<Option<String>>,
) -> anyhow::Result<Vec<u8>> {
    const CHUNK_BYTES: usize = 8 * 1024;
    let mut output = Vec::new();
    let mut buffer = [0_u8; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(output);
        }
        let next_stream = (output.len() as u64).saturating_add(read as u64);
        let mut aggregate = aggregate.lock().await;
        let next_aggregate = aggregate.saturating_add(read as u64);
        if next_stream > SUBPROCESS_STREAM_LIMIT || next_aggregate > SUBPROCESS_AGGREGATE_LIMIT {
            let _ = failure.send(Some(format!(
                "{stream} exceeded bounded subprocess output limit"
            )));
            return Ok(output);
        }
        output.reserve(read);
        output.extend_from_slice(&buffer[..read]);
        *aggregate = next_aggregate;
    }
}

async fn settle_bounded_output(
    spawned: SpawnedBoundedOutput,
    timeout: Duration,
    cancellation: &Cancellation,
    description: &str,
) -> Result<std::process::Output, BoundedOutputFailure> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| BoundedOutputFailure::unsettled(anyhow::anyhow!("deadline overflow")))?;
    let SpawnedBoundedOutput {
        mut child,
        identity,
        stdout,
        stderr,
        failure,
    } = spawned;
    let stdout_abort = stdout.abort_handle();
    let stderr_abort = stderr.abort_handle();
    let mut failure_signal = failure.clone();
    let result = match tokio::select! {
        status = child.wait() => status.context("wait for bounded subprocess").map(Some),
        _ = wait_for_stream_failure(&mut failure_signal) => Ok(None),
        () = cancellation.cancelled() => Ok(None),
        () = tokio::time::sleep(timeout) => Ok(None),
    } {
        Ok(result) => result,
        Err(error) => {
            let containment = terminate_process_group(&identity, &mut child).await;
            stdout_abort.abort();
            stderr_abort.abort();
            return Err(BoundedOutputFailure::unsettled(
                containment.err().unwrap_or(error),
            ));
        }
    };
    let containment = if result.is_none() {
        terminate_process_group(&identity, &mut child).await
    } else {
        terminate_owned_process_group(&identity).await
    };
    if let Err(error) = containment {
        stdout_abort.abort();
        stderr_abort.abort();
        return Err(BoundedOutputFailure::unsettled(error));
    }
    let readers = async move {
        let (stdout, stderr) = tokio::try_join!(
            async { stdout.await.context("bounded stdout task failed")? },
            async { stderr.await.context("bounded stderr task failed")? },
        )?;
        Ok::<_, anyhow::Error>((stdout, stderr))
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let cancellation_already_latched = cancellation.requested();
    let joined = tokio::select! {
        result = tokio::time::timeout(remaining, readers) => result,
        () = cancellation.cancelled(), if !cancellation_already_latched => {
            stdout_abort.abort();
            stderr_abort.abort();
            return Err(BoundedOutputFailure::unsettled(anyhow::anyhow!(
                "{description} was cancelled while settling output"
            )));
        }
    };
    let (stdout, stderr) = match joined {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            stdout_abort.abort();
            stderr_abort.abort();
            return Err(BoundedOutputFailure::settled(error));
        }
        Err(_) => {
            let containment = terminate_owned_process_group(&identity).await;
            stdout_abort.abort();
            stderr_abort.abort();
            return Err(match containment {
                Ok(()) => BoundedOutputFailure::unsettled(anyhow::anyhow!(
                    "{description} output readers exceeded operation deadline"
                )),
                Err(error) => BoundedOutputFailure::unsettled(error),
            });
        }
    };
    if let Some(error) = failure.borrow().clone() {
        return Err(BoundedOutputFailure::settled(anyhow::anyhow!(
            "{description} failed: {error}"
        )));
    }
    if cancellation.requested() {
        return Err(BoundedOutputFailure::settled(anyhow::anyhow!(
            "{description} was cancelled"
        )));
    }
    let status = result
        .with_context(|| format!("{description} deadline exceeded"))
        .map_err(BoundedOutputFailure::settled)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[derive(Debug)]
struct BoundedOutputFailure {
    error: anyhow::Error,
    helper_settled: bool,
}

impl BoundedOutputFailure {
    fn settled(error: anyhow::Error) -> Self {
        Self {
            error,
            helper_settled: true,
        }
    }

    fn unsettled(error: anyhow::Error) -> Self {
        Self {
            error,
            helper_settled: false,
        }
    }
}

async fn wait_for_stream_failure(failure: &mut watch::Receiver<Option<String>>) {
    while failure.borrow().is_none() {
        if failure.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalledEffect {
    ReadOnly,
    Mutation,
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_output_journalled(
    command: &Path,
    args: &[String],
    deadline: &Deadline,
    effects: &ExecutionCancellationContext,
    operation_kind: &str,
    resource_identity: &str,
    effect: JournalledEffect,
    description: &str,
) -> anyhow::Result<std::process::Output> {
    execute_output_journalled_with_environment(
        command,
        args,
        deadline,
        effects,
        operation_kind,
        resource_identity,
        effect,
        description,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_output_journalled_inherited_environment(
    command: &Path,
    args: &[String],
    deadline: &Deadline,
    effects: &ExecutionCancellationContext,
    operation_kind: &str,
    resource_identity: &str,
    effect: JournalledEffect,
    description: &str,
) -> anyhow::Result<std::process::Output> {
    execute_output_journalled_with_environment(
        command,
        args,
        deadline,
        effects,
        operation_kind,
        resource_identity,
        effect,
        description,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_output_journalled_with_environment(
    command: &Path,
    args: &[String],
    deadline: &Deadline,
    effects: &ExecutionCancellationContext,
    operation_kind: &str,
    resource_identity: &str,
    effect: JournalledEffect,
    description: &str,
    clear_environment: bool,
) -> anyhow::Result<std::process::Output> {
    let remaining = deadline
        .remaining()
        .with_context(|| format!("{description} requires an absolute deadline"))?;
    anyhow::ensure!(!remaining.is_zero(), "{description} deadline exceeded");
    effects.check()?;
    let operation = effects.begin_external_operation(operation_kind, resource_identity)?;
    let spawned = spawn_bounded_output(command, args, description, clear_environment)?;
    let helper = effects.begin_external_operation(
        "journalled_subprocess_helper_process",
        &format_process_effect_identity(&spawned.identity),
    )?;
    let settled =
        settle_bounded_output(spawned, remaining, &effects.cancellation, description).await;
    match settled {
        Ok(output) => {
            helper.reconciled()?;
            match effect {
                JournalledEffect::ReadOnly => operation.reconciled()?,
                JournalledEffect::Mutation => operation.completed()?,
            }
            Ok(output)
        }
        Err(failure) => {
            if failure.helper_settled {
                helper.reconciled()?;
            } else {
                helper.ambiguous("journalled_helper_settlement_uncertain")?;
            }
            match effect {
                JournalledEffect::Mutation => {
                    operation.ambiguous("journalled_mutation_outcome_unknown")?
                }
                JournalledEffect::ReadOnly if failure.helper_settled => operation.reconciled()?,
                JournalledEffect::ReadOnly => {
                    operation.ambiguous("journalled_read_settlement_uncertain")?
                }
            }
            Err(failure.error)
        }
    }
}

fn validate_image_identity(identity: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        identity.len() == 71
            && identity.starts_with("sha256:")
            && identity[7..]
                .chars()
                .all(|character| character.is_ascii_hexdigit()),
        "resolved image identity has invalid shape"
    );
    Ok(())
}

pub fn verify_image_identity(expected: &str, current: &str) -> anyhow::Result<()> {
    validate_image_identity(expected)?;
    validate_image_identity(current)?;
    anyhow::ensure!(
        expected == current,
        "configured image changed during execution setup: expected {expected}, resolved {current}"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    None,
    Named(String),
    Unrestricted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestrictedEgress {
    pub network_name: String,
    pub proxy_url: String,
    pub no_proxy: String,
    pub proxy_host: String,
    pub proxy_ip: String,
    pub allowed_hosts: Vec<String>,
}

impl RestrictedEgress {
    pub fn apply(&self, args: &mut Vec<String>, environment: &mut HashMap<String, String>) {
        args.push(format!("--network={}", self.network_name));
        args.push("--dns=127.0.0.1".to_string());
        args.push(format!("--add-host={}:{}", self.proxy_host, self.proxy_ip));
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            environment.insert(name.to_string(), self.proxy_url.clone());
        }
        for name in ["ALL_PROXY", "all_proxy"] {
            environment.insert(name.to_string(), String::new());
        }
        for name in ["NO_PROXY", "no_proxy"] {
            environment.insert(name.to_string(), self.no_proxy.clone());
        }
        environment.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            self.allowed_hosts.join(","),
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct CredentialMaterial {
    pub environment: BTreeMap<String, String>,
    pub mounts: Vec<DockerMount>,
    pub temporary_files: Vec<PathBuf>,
    pub redaction_values: Vec<String>,
}

impl CredentialMaterial {
    pub fn apply(&self, args: &mut Vec<String>, environment: &mut HashMap<String, String>) {
        for mount in &self.mounts {
            args.push("--mount".to_string());
            args.push(mount.bind_argument());
        }
        for (name, value) in &self.environment {
            environment
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopedHelper {
    pub mount: DockerMount,
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct DockerExecutionPlan {
    pub image_identity: String,
    pub container_name: String,
    pub entrypoint: String,
    pub command_args: Vec<String>,
    pub mounts: Vec<DockerMount>,
    pub environment: BTreeMap<String, String>,
    pub network: NetworkMode,
    pub dns: Vec<String>,
    pub extra_hosts: Vec<String>,
    pub ownership: DockerOwnership,
    pub user: String,
    pub workdir: Option<String>,
    pub memory_limit: String,
    pub cpu_limit: String,
    pub pids_limit: u32,
    pub read_only: bool,
    pub tmpfs: Vec<String>,
}

impl DockerExecutionPlan {
    pub fn materialize_args(&self) -> anyhow::Result<Vec<String>> {
        validate_image_identity(&self.image_identity)?;
        anyhow::ensure!(
            !self.container_name.is_empty(),
            "Docker container name is empty"
        );
        anyhow::ensure!(!self.entrypoint.is_empty(), "Docker entrypoint is empty");
        anyhow::ensure!(!self.user.is_empty(), "Docker runtime user is empty");
        anyhow::ensure!(
            !self.memory_limit.is_empty(),
            "Docker memory limit is empty"
        );
        anyhow::ensure!(!self.cpu_limit.is_empty(), "Docker CPU limit is empty");
        anyhow::ensure!(self.pids_limit > 0, "Docker PID limit must be positive");
        if let Some(workdir) = &self.workdir {
            validate_container_path(workdir)?;
        }
        for tmpfs in &self.tmpfs {
            let path = tmpfs.split(':').next().unwrap_or_default();
            validate_container_path(path)?;
        }
        let mut destinations = std::collections::BTreeSet::new();
        for mount in &self.mounts {
            anyhow::ensure!(
                destinations.insert(&mount.container_path),
                "duplicate Docker mount destination '{}'",
                mount.container_path
            );
        }
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            self.container_name.clone(),
            "--user".to_string(),
            self.user.clone(),
            "--security-opt=no-new-privileges".to_string(),
            "--cap-drop=ALL".to_string(),
            format!("--memory={}", self.memory_limit),
            format!("--cpus={}", self.cpu_limit),
            format!("--pids-limit={}", self.pids_limit),
        ];
        if self.read_only {
            args.push("--read-only".to_string());
        }
        for tmpfs in &self.tmpfs {
            args.push("--tmpfs".to_string());
            args.push(tmpfs.clone());
        }
        if let Some(workdir) = &self.workdir {
            args.push("--workdir".to_string());
            args.push(workdir.clone());
        }
        match &self.network {
            NetworkMode::None => args.push("--network=none".to_string()),
            NetworkMode::Named(name) => {
                anyhow::ensure!(!name.is_empty(), "Docker network name is empty");
                args.push(format!("--network={name}"));
            }
            NetworkMode::Unrestricted => {}
        }
        for dns in &self.dns {
            anyhow::ensure!(!dns.is_empty(), "Docker DNS server is empty");
            args.push(format!("--dns={dns}"));
        }
        for host in &self.extra_hosts {
            anyhow::ensure!(
                host.split_once(':').is_some(),
                "Docker extra host is malformed"
            );
            args.push(format!("--add-host={host}"));
        }
        args.extend(self.ownership.label_args());
        for mount in &self.mounts {
            args.push("--mount".to_string());
            args.push(mount.bind_argument());
        }
        for (name, value) in &self.environment {
            args.push("-e".to_string());
            args.push(format!("{name}={value}"));
        }
        args.extend([
            "--entrypoint".to_string(),
            self.entrypoint.clone(),
            self.image_identity.clone(),
        ]);
        args.extend(self.command_args.clone());
        Ok(args)
    }
}

pub fn cleanup_temporary_files(paths: &[PathBuf]) -> CleanupStatus {
    let failures = paths
        .iter()
        .filter_map(|path| match std::fs::remove_file(path) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(format!("{}: {error}", path.display())),
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        CleanupStatus::Complete
    } else {
        CleanupStatus::Failed(failures.join("; "))
    }
}

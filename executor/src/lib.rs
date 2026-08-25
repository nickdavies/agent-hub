use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

pub mod credential_provider;
pub mod egress;
mod process;

pub use process::{ProcessIdentity, read_process_identity, verify_process_group_absent};

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
    Started,
    Completed,
    Ambiguous,
    CleanupFailed,
    Reconciled,
}

#[derive(Debug, Clone)]
pub struct ExecutionCancellationContext {
    cancellation: Cancellation,
    journal_path: Arc<PathBuf>,
    journal_lock: Arc<std::sync::Mutex<()>>,
    effect_state: Arc<std::sync::Mutex<HashMap<uuid::Uuid, ExternalEffectStatus>>>,
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
                effect_state.insert(record.operation_id, record.status);
            }
        }
        Ok(Self {
            cancellation,
            journal_path: Arc::new(journal_path),
            journal_lock: Arc::new(std::sync::Mutex::new(())),
            effect_state: Arc::new(std::sync::Mutex::new(effect_state)),
        })
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
            state.values().all(|status| matches!(
                status,
                ExternalEffectStatus::Completed | ExternalEffectStatus::Reconciled
            )),
            "external effects remain unresolved"
        );
        drop(state);
        let root = self
            .journal_path
            .parent()
            .context("effect journal has no parent")?;
        let parent = root.parent().context("effect journal root has no parent")?;
        std::fs::remove_dir_all(root)?;
        if std::fs::read_dir(parent)?.next().is_none() {
            let grandparent = parent
                .parent()
                .context("effect journal namespace root has no parent")?;
            std::fs::remove_dir(parent)?;
            return sync_directory(grandparent);
        }
        sync_directory(parent)
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
            ExternalEffectStatus::Started,
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
            .insert(operation_id, status);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path, receiver: watch::Receiver<bool>) -> anyhow::Result<Self> {
        Self::new(root, Cancellation::new(receiver))
    }
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
        self.finish(ExternalEffectStatus::Completed)
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStatus {
    Exited(i32),
    TimedOut,
    Cancelled,
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
    let transcript = Arc::new(Mutex::new(tokio::io::BufWriter::new(
        tokio::fs::File::from_std(transcript_file),
    )));

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
    let stdout = child
        .stdout
        .take()
        .context("capture resolved command stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capture resolved command stderr")?;
    let stdout_task = tokio::spawn(persist_stream(
        stdout,
        tokio::fs::File::from_std(stdout_file),
        Arc::clone(&transcript),
        "stdout",
        request.redaction_values.clone(),
    ));
    let stderr_task = tokio::spawn(persist_stream(
        stderr,
        tokio::fs::File::from_std(stderr_file),
        Arc::clone(&transcript),
        "stderr",
        request.redaction_values.clone(),
    ));

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let status = loop {
        tokio::select! {
            waited = child.wait() => {
                let status = waited.context("wait for resolved command")?;
                break ExecutionStatus::Exited(status.code().unwrap_or(1));
            }
            _ = ticker.tick() => {
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
    stdout_task
        .await
        .context("stdout persistence task failed")??;
    stderr_task
        .await
        .context("stderr persistence task failed")??;
    verify_process_group_absent(&process_identity).await?;
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
    reader: R,
    output: tokio::fs::File,
    transcript: Arc<Mutex<tokio::io::BufWriter<tokio::fs::File>>>,
    stream: &'static str,
    redaction_values: Vec<String>,
) -> anyhow::Result<()> {
    let mut lines = BufReader::new(reader).lines();
    let mut output = tokio::io::BufWriter::new(output);
    while let Some(line) = lines.next_line().await? {
        let line = redaction_values.iter().fold(line, |line, value| {
            if value.is_empty() {
                line
            } else {
                line.replace(value, "[REDACTED]")
            }
        });
        output.write_all(line.as_bytes()).await?;
        output.write_all(b"\n").await?;
        output.flush().await?;
        let mut record = serde_json::to_vec(&serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "stream": stream,
            "line": line,
        }))?;
        record.push(b'\n');
        let mut transcript = transcript.lock().await;
        transcript.write_all(&record).await?;
        transcript.flush().await?;
    }
    Ok(())
}

async fn terminate_process_group(
    identity: &ProcessIdentity,
    child: &mut tokio::process::Child,
) -> anyhow::Result<()> {
    verify_process_group_absent(identity).await?;
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
}

impl DockerCli {
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
        }
    }

    pub async fn inspect_ownership(
        &self,
        name: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<BTreeMap<String, String>>> {
        let mut command = tokio::process::Command::new(&self.command);
        command.env_clear().args([
            "container",
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            name,
        ]);
        command.kill_on_drop(true);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .context("timed out inspecting named container")??;
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
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("container cleanup deadline overflow")?;
        let actual = self.inspect_ownership(name, remaining(deadline)?).await?;
        let Some(actual) = actual else { return Ok(()) };
        ensure_expected_labels(name, expected, &actual)?;
        let mut remove = tokio::process::Command::new(&self.command);
        remove
            .env_clear()
            .args(["rm", "-f", name])
            .kill_on_drop(true);
        let output = tokio::time::timeout(remaining(deadline)?, remove.output())
            .await
            .context("timed out removing owned container")??;
        anyhow::ensure!(
            output.status.success(),
            "docker rm -f {name} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        match self.inspect_ownership(name, remaining(deadline)?).await? {
            None => Ok(()),
            Some(actual) => {
                ensure_expected_labels(name, expected, &actual)?;
                anyhow::bail!("owned container '{name}' remains after removal")
            }
        }
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
) -> anyhow::Result<String> {
    let remaining = deadline
        .remaining()
        .context("image resolution requires an absolute deadline")?;
    anyhow::ensure!(!remaining.is_zero(), "image resolution deadline exceeded");
    let mut command = tokio::process::Command::new(docker_command);
    command
        .env_clear()
        .args(["image", "inspect", "--format", "{{.Id}}", configured_image]);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(remaining, command.output())
        .await
        .context("image resolution deadline exceeded")??;
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
    let remaining = deadline
        .remaining()
        .with_context(|| format!("{description} requires an absolute deadline"))?;
    anyhow::ensure!(!remaining.is_zero(), "{description} deadline exceeded");
    let mut process = tokio::process::Command::new(command);
    process.env_clear().args(args).kill_on_drop(true);
    tokio::time::timeout(remaining, process.output())
        .await
        .with_context(|| format!("{description} deadline exceeded"))?
        .with_context(|| format!("execute {description}"))
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

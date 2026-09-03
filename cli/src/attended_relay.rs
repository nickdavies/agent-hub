use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_hub_executor::ExecutionCancellationContext;
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use protocol::{
    ATTENDED_GATEWAY_MAX_FRAME_BYTES, Approval, ApprovalDecision, ApprovalResolveRequest,
    ApprovalResponse, ApprovalStatus, ApprovalWaitResponse, AttendedGatewayBindingV1,
    AttendedGatewayDecisionKind, AttendedGatewayRequestV1, Secret, decode_attended_gateway_payload,
    encode_attended_gateway_frame,
};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore, watch};
use url::Url;
use uuid::Uuid;

const MAX_CONNECTIONS: usize = 32;
const MAX_IN_FLIGHT_APPROVALS: usize = 16;
const MAX_REPLAY_IDS: usize = 4096;
const MAX_LEDGER_TRANSITIONS: usize = 32_768;
const MAX_LEDGER_BYTES: usize = 16 * 1024 * 1024;
const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_SLICE: Duration = Duration::from_secs(60);
const CANCELLATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(target_os = "linux")]
const UNIX_SOCKET_PATH_CAPACITY: usize = 108;
#[cfg(target_os = "linux")]
const MAX_RELAY_SOCKET_PATH_BYTES: usize =
    "/run/user/".len() + 10 + "/agent-hub/".len() + 36 + "/approval.sock".len();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelaySocketLocation {
    pub runtime_dir: PathBuf,
    pub socket_root: PathBuf,
    pub socket_path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct AttendedRelayConfig {
    pub socket_path: PathBuf,
    pub binding: AttendedGatewayBindingV1,
    pub approval_bundle: protocol::AttendedApprovalBundleIdentity,
    pub run_deadline: DateTime<Utc>,
    pub server_url: String,
    pub token: Option<Secret>,
    pub docker_path: PathBuf,
    pub container_name: String,
    pub ledger_path: PathBuf,
    pub plugin_nonce: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

#[derive(Clone)]
struct PendingApproval {
    approval_id: Uuid,
}

struct RelayState {
    request_ids: HashSet<String>,
    pending: HashMap<String, PendingApproval>,
    ready: bool,
    calls: HashMap<String, CallBinding>,
    entries: HashMap<String, LedgerEntry>,
    transitions: usize,
    ledger_bytes: usize,
    ledger_file: File,
}

#[derive(Clone, PartialEq, Eq)]
struct CallBinding {
    call_id: String,
    session_id: String,
    tool_name: String,
    raw_args_digest: String,
    transcript_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LedgerStatus {
    Created,
    LocalAllow,
    LocalDeny,
    RemoteAllow,
    RemoteDeny,
    RemoteCancel,
    Completed,
    Failed,
}

struct LedgerEntry {
    call: CallBinding,
    canonical_args_digest: String,
    status: LedgerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisionOutcome {
    NoDenials,
    Denied,
    Cancelled,
}

#[derive(Clone)]
struct RelayRuntime {
    config: AttendedRelayConfig,
    endpoint: Url,
    client: Client,
    state: Arc<Mutex<RelayState>>,
    effects: ExecutionCancellationContext,
    approvals: Arc<Semaphore>,
    socket_identity: SocketIdentity,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationEffect {
    request_id: String,
    request: AttendedGatewayRequestV1,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalStateEffect {
    request_id: String,
    approval_id: Uuid,
    status: ApprovalStatus,
}

pub(crate) struct AttendedRelay {
    pub(crate) socket_path: PathBuf,
    identity: SocketIdentity,
    socket_root: PathBuf,
    root_identity: DirectoryIdentity,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    runtime: RelayRuntime,
}

#[cfg(target_os = "linux")]
pub(crate) fn host_relay_socket_location(run_epoch: Uuid) -> anyhow::Result<RelaySocketLocation> {
    use std::os::unix::ffi::OsStrExt as _;

    ensure!(
        MAX_RELAY_SOCKET_PATH_BYTES < UNIX_SOCKET_PATH_CAPACITY,
        "maximum attended relay socket path exceeds sockaddr_un.sun_path"
    );
    let uid = unsafe { libc::geteuid() };
    let expected = PathBuf::from(format!("/run/user/{uid}"));
    if let Some(configured) = std::env::var_os("XDG_RUNTIME_DIR") {
        ensure!(
            Path::new(&configured) == expected,
            "XDG_RUNTIME_DIR is not the owner runtime directory"
        );
    }
    verify_runtime_directory(&expected, uid)?;
    let socket_root = expected.join("agent-hub").join(run_epoch.to_string());
    let socket_path = socket_root.join("approval.sock");
    ensure!(
        socket_path.as_os_str().as_bytes().len() < UNIX_SOCKET_PATH_CAPACITY,
        "attended relay socket path exceeds sockaddr_un.sun_path"
    );
    Ok(RelaySocketLocation {
        runtime_dir: expected,
        socket_root,
        socket_path,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn host_relay_socket_location(_run_epoch: Uuid) -> anyhow::Result<RelaySocketLocation> {
    anyhow::bail!("secure attended relay runtime directories require Linux")
}

#[cfg(target_os = "linux")]
fn verify_runtime_directory(path: &Path, expected_uid: u32) -> anyhow::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect owner runtime directory {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "owner runtime directory is not a real directory"
    );
    ensure!(
        std::fs::canonicalize(path)? == path,
        "owner runtime directory is not canonical"
    );
    ensure!(
        metadata.uid() == expected_uid,
        "owner runtime directory owner mismatch"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o700,
        "owner runtime directory mode is not 0700"
    );
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::statfs(path.as_ptr(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect owner runtime filesystem");
    }
    let filesystem = unsafe { filesystem.assume_init() };
    ensure!(
        filesystem.f_type as libc::c_long == libc::TMPFS_MAGIC as libc::c_long,
        "owner runtime directory is not on a runtime tmpfs"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_runtime_directory(_path: &Path, _expected_uid: u32) -> anyhow::Result<()> {
    anyhow::bail!("secure attended relay runtime directories require Linux")
}

impl AttendedRelay {
    pub(crate) async fn start(
        config: AttendedRelayConfig,
        effects: &ExecutionCancellationContext,
        fail_closed: watch::Sender<bool>,
    ) -> anyhow::Result<Self> {
        let endpoint = validated_endpoint(&config.server_url, config.token.is_some())?;
        ensure!(
            config.approval_bundle.digest == config.binding.approval_bundle_digest
                && config.approval_bundle.protocol == protocol::APPROVAL_BUNDLE_PROTOCOL,
            "approval relay bundle identity does not match the authorized binding"
        );
        ensure!(
            config.socket_path.is_absolute(),
            "approval relay socket path is not absolute"
        );
        verify_socket_path_length(&config.socket_path)?;
        let socket_root = config
            .socket_path
            .parent()
            .context("approval relay socket has no parent")?
            .to_path_buf();
        let root_identity = prepare_socket_root(&socket_root, effects)?;
        verify_socket_parent(&config.socket_path)?;
        let listener = match bind_listener(&config.socket_path, effects) {
            Ok(listener) => listener,
            Err(error) => {
                if std::fs::symlink_metadata(&config.socket_path)
                    .is_err_and(|inspect| inspect.kind() == std::io::ErrorKind::NotFound)
                {
                    remove_socket_root(&socket_root, Some(root_identity), effects)?;
                }
                return Err(error);
            }
        };
        let bound_identity = socket_identity(&config.socket_path)?;
        let chmod = effects.begin_external_operation(
            "attended_relay_socket_chmod",
            &config.socket_path.to_string_lossy(),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(error) = std::fs::set_permissions(
                &config.socket_path,
                std::fs::Permissions::from_mode(0o600),
            ) {
                drop(listener);
                remove_socket_if_exact(&config.socket_path, bound_identity, effects)?;
                chmod.reconciled()?;
                remove_socket_root(&socket_root, Some(root_identity), effects)?;
                return Err(error).context("set approval relay socket permissions");
            }
        }
        chmod.completed()?;
        let identity = verify_socket(&config.socket_path, None)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(HTTP_OPERATION_TIMEOUT)
            .timeout(HTTP_OPERATION_TIMEOUT)
            .build()?;
        let ledger_file = create_private_ledger(&config.ledger_path)?;
        let runtime = RelayRuntime {
            config: config.clone(),
            endpoint,
            client,
            state: Arc::new(Mutex::new(RelayState {
                request_ids: HashSet::new(),
                pending: HashMap::new(),
                ready: false,
                calls: HashMap::new(),
                entries: HashMap::new(),
                transitions: 0,
                ledger_bytes: 0,
                ledger_file,
            })),
            effects: effects.clone(),
            approvals: Arc::new(Semaphore::new(MAX_IN_FLIGHT_APPROVALS)),
            socket_identity: identity,
        };
        let (shutdown, shutdown_rx) = watch::channel(false);
        let loop_runtime = runtime.clone();
        let task = tokio::spawn(async move {
            let result = relay_loop(listener, identity, loop_runtime, shutdown_rx).await;
            if result.is_err() {
                let _ = fail_closed.send(true);
            }
            result
        });
        Ok(Self {
            socket_path: config.socket_path,
            identity,
            socket_root,
            root_identity,
            shutdown,
            task: Some(task),
            runtime,
        })
    }

    pub(crate) fn verify_for_mount(&self) -> anyhow::Result<()> {
        verify_socket(&self.socket_path, Some(self.identity))?;
        Ok(())
    }

    pub(crate) fn binding(&self) -> &AttendedGatewayBindingV1 {
        &self.runtime.config.binding
    }

    pub(crate) fn deadline(&self) -> DateTime<Utc> {
        self.runtime.config.run_deadline
    }

    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        let _ = self.shutdown.send(true);
        let task_result = match self.task.take() {
            Some(task) => task
                .await
                .context("approval relay task panicked")
                .and_then(|result| result),
            None => Ok(()),
        };
        let pending_result = tokio::time::timeout(CANCELLATION_DRAIN_TIMEOUT, async {
            cancel_all_pending(&self.runtime).await?;
            recover_approval_effects(
                &self.runtime.client,
                &self.runtime.endpoint,
                self.runtime.config.token.as_ref(),
                &self.runtime.effects,
            )
            .await
        })
        .await
        .context("pending approval cancellation drain timed out")
        .and_then(|result| result);
        let socket_result = remove_socket(&self.socket_path, self.identity, &self.runtime.effects)
            .and_then(|()| {
                remove_socket_root(
                    &self.socket_root,
                    Some(self.root_identity),
                    &self.runtime.effects,
                )
            });
        let mut failures = Vec::new();
        for (label, result) in [
            ("listener", task_result),
            ("pending approvals", pending_result),
            ("socket", socket_result),
        ] {
            if let Err(error) = result {
                failures.push(format!("{label}: {error:#}"));
            }
        }
        ensure!(
            failures.is_empty(),
            "attended relay cleanup failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    pub(crate) async fn supervision_outcome(
        &self,
        cancelled: bool,
    ) -> anyhow::Result<SupervisionOutcome> {
        let state = self.runtime.state.lock().await;
        supervision_outcome(&state, cancelled)
    }
}

fn supervision_outcome(state: &RelayState, cancelled: bool) -> anyhow::Result<SupervisionOutcome> {
    if !cancelled {
        ensure!(
            state.ready,
            "supervised OpenCode plugin never became host-ready"
        );
    }
    let denied = state.entries.values().any(|entry| {
        matches!(
            entry.status,
            LedgerStatus::LocalDeny | LedgerStatus::RemoteDeny | LedgerStatus::RemoteCancel
        )
    });
    if !cancelled {
        for call in state.calls.values().filter(|call| !call.transcript_only) {
            let statuses = state
                .entries
                .values()
                .filter(|entry| &entry.call == call)
                .map(|entry| entry.status)
                .collect::<Vec<_>>();
            let call_denied = statuses.iter().any(|status| {
                matches!(
                    status,
                    LedgerStatus::LocalDeny | LedgerStatus::RemoteDeny | LedgerStatus::RemoteCancel
                )
            });
            ensure!(
                statuses.iter().all(|status| {
                    *status == LedgerStatus::Completed
                        || call_denied
                            && matches!(
                                status,
                                LedgerStatus::LocalDeny
                                    | LedgerStatus::RemoteDeny
                                    | LedgerStatus::RemoteCancel
                                    | LedgerStatus::Failed
                            )
                }),
                "supervised OpenCode run has unresolved, failed, or missing routed calls"
            );
        }
    }
    if denied {
        return Ok(SupervisionOutcome::Denied);
    }
    if cancelled {
        return Ok(SupervisionOutcome::Cancelled);
    }
    Ok(SupervisionOutcome::NoDenials)
}

#[cfg(unix)]
fn create_private_ledger(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let parent = path.parent().context("supervision ledger has no parent")?;
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "supervision ledger parent is not a real directory"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "supervision ledger parent owner mismatch"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "supervision ledger parent is not private"
    );
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("create private supervision ledger {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "supervision ledger is not a regular file"
    );
    Ok(file)
}

#[cfg(not(unix))]
fn create_private_ledger(_path: &Path) -> anyhow::Result<File> {
    anyhow::bail!("private supervision ledgers require Unix")
}

#[cfg(target_os = "linux")]
fn verify_socket_path_length(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    ensure!(
        path.as_os_str().as_bytes().len() < UNIX_SOCKET_PATH_CAPACITY,
        "attended relay socket path exceeds sockaddr_un.sun_path"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_socket_path_length(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("secure attended relay socket paths require Linux")
}

fn bind_listener(
    path: &Path,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<UnixListener> {
    let bind =
        effects.begin_external_operation("attended_relay_socket_bind", &path.to_string_lossy())?;
    match UnixListener::bind(path) {
        Ok(listener) => {
            bind.completed()?;
            Ok(listener)
        }
        Err(error) => {
            let absence = std::fs::symlink_metadata(path)
                .is_err_and(|inspect| inspect.kind() == std::io::ErrorKind::NotFound);
            if absence || error.kind() == std::io::ErrorKind::AddrInUse {
                bind.reconciled()?;
            } else {
                bind.ambiguous("attended_relay_bind_outcome_unknown")?;
            }
            Err(error).with_context(|| format!("bind approval relay socket {}", path.display()))
        }
    }
}

pub(crate) async fn recover_attended_relay(
    socket_path: &Path,
    endpoint: &str,
    token: Option<&Secret>,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let endpoint = validated_endpoint(endpoint, token.is_some())?;
    let client = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(HTTP_OPERATION_TIMEOUT)
        .timeout(HTTP_OPERATION_TIMEOUT)
        .build()?;
    recover_approval_effects(&client, &endpoint, token, effects).await?;
    recover_attended_relay_socket(socket_path, effects)
}

async fn recover_approval_effects(
    client: &Client,
    endpoint: &Url,
    token: Option<&Secret>,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let registrations = effects.unresolved_operation_resources("attended_relay_approval_register");
    ensure!(
        registrations.len() <= MAX_REPLAY_IDS,
        "attended relay recovery state exceeds limit"
    );
    for resource in registrations {
        let registration: RegistrationEffect = serde_json::from_str(&resource)
            .context("invalid attended approval registration effect")?;
        let response: ApprovalResponse = send_json(
            authenticated(
                token,
                client.post(endpoint_url(endpoint, "api/v1/hooks/approval")?),
            )
            .json(&registration.request.approval),
            StatusCode::OK,
        )
        .await?;
        persist_approval_state(
            effects,
            &registration.request_id,
            response.id,
            &response.status,
        )?;
        let status = if response.status == ApprovalStatus::Pending {
            cancel_remote(
                client,
                endpoint,
                token,
                response.id,
                effects,
                &registration.request_id,
            )
            .await?
        } else {
            response.status
        };
        ensure!(status.is_resolved(), "recovered approval is not terminal");
        verify_terminal(
            client,
            endpoint,
            token,
            response.id,
            &registration.request_id,
        )
        .await?;
        reconcile_request_effects(effects, &registration.request_id)?;
    }
    let states = effects.unresolved_operation_resources("attended_relay_approval_state");
    ensure!(
        states.len() <= MAX_REPLAY_IDS,
        "attended relay persisted approval state exceeds limit"
    );
    let mut recovered = HashSet::new();
    for resource in states {
        let state: ApprovalStateEffect =
            serde_json::from_str(&resource).context("invalid attended approval state effect")?;
        if !recovered.insert(state.request_id.clone()) {
            continue;
        }
        let current = get_approval(client, endpoint, token, state.approval_id).await?;
        if current.status == ApprovalStatus::Pending {
            cancel_remote(
                client,
                endpoint,
                token,
                state.approval_id,
                effects,
                &state.request_id,
            )
            .await?;
        }
        verify_terminal(
            client,
            endpoint,
            token,
            state.approval_id,
            &state.request_id,
        )
        .await?;
        reconcile_request_effects(effects, &state.request_id)?;
    }
    Ok(())
}

pub(crate) fn recover_attended_relay_socket(
    socket_path: &Path,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let socket_resource = socket_path.to_string_lossy();
    let socket_owned = [
        "attended_relay_socket_bind",
        "attended_relay_socket_chmod",
        "attended_relay_socket_remove",
    ]
    .into_iter()
    .any(|kind| {
        effects
            .unresolved_operation_resources(kind)
            .iter()
            .any(|resource| resource == socket_resource.as_ref())
    });
    match std::fs::symlink_metadata(socket_path) {
        Ok(_) => {
            ensure!(
                socket_owned,
                "approval relay socket has no run-owned effect"
            );
            socket_identity(socket_path)?;
            let remove = effects.begin_external_operation(
                "attended_relay_socket_recovery_remove",
                &socket_resource,
            )?;
            std::fs::remove_file(socket_path)?;
            remove.completed()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        std::fs::symlink_metadata(socket_path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "approval relay socket remains after recovery"
    );
    for kind in [
        "attended_relay_socket_bind",
        "attended_relay_socket_chmod",
        "attended_relay_socket_remove",
        "attended_relay_socket_recovery_remove",
    ] {
        effects.reconcile_operation_kind_resource(kind, &socket_resource)?;
    }
    let socket_root = socket_path
        .parent()
        .context("approval relay socket has no parent")?;
    let root_resource = socket_root.to_string_lossy();
    let root_owned = effects
        .unresolved_operation_resources("attended_relay_socket_root_create")
        .iter()
        .any(|resource| resource == root_resource.as_ref());
    match std::fs::symlink_metadata(socket_root) {
        Ok(_) if root_owned => {
            verify_private_directory(socket_root, None)?;
            let remove = effects.begin_external_operation(
                "attended_relay_socket_root_recovery_remove",
                &root_resource,
            )?;
            std::fs::remove_dir(socket_root)?;
            remove.completed()?;
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    remove_socket_root(socket_root, None, effects)?;
    Ok(())
}

impl Drop for AttendedRelay {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = self.runtime.effects.begin_external_operation(
                "attended_relay_unawaited_drop",
                &self.socket_path.to_string_lossy(),
            );
        }
    }
}

async fn relay_loop(
    listener: UnixListener,
    identity: SocketIdentity,
    runtime: RelayRuntime,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = tokio::task::JoinSet::new();
    let mut integrity = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept attended approval connection")?;
                verify_socket(&runtime.config.socket_path, Some(identity))?;
                let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let runtime = runtime.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    handle_connection(stream, runtime).await
                });
            }
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(result) = result {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            return Err(error).context("attended relay connection failed");
                        }
                        Err(error) => {
                            return Err(error).context("attended relay connection panicked");
                        }
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = integrity.tick() => {
                verify_socket(&runtime.config.socket_path, Some(identity))?;
            }
        }
    }
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            return Err(error).context("attended relay connection panicked");
        }
    }
    Ok(())
}

async fn handle_connection(mut stream: UnixStream, runtime: RelayRuntime) -> anyhow::Result<()> {
    let payload: serde_json::Value = read_frame(&mut stream).await?;
    if payload.get("version").and_then(serde_json::Value::as_str)
        == Some(protocol::ATTENDED_GATEWAY_TRANSITION_VERSION)
    {
        let transition: protocol::AttendedGatewayTransitionV1 = serde_json::from_value(payload)?;
        transition
            .validate(&runtime.config.binding, &runtime.config.plugin_nonce)
            .map_err(anyhow::Error::msg)?;
        record_sandbox_transition(&runtime, &transition).await?;
        write_frame(&mut stream, &transition.ack()).await?;
        return Ok(());
    }
    let request: AttendedGatewayRequestV1 = serde_json::from_value(payload)?;
    request
        .validate(&runtime.config.binding, Utc::now())
        .map_err(anyhow::Error::msg)?;
    ensure!(
        request.deadline <= runtime.config.run_deadline,
        "approval deadline exceeds run deadline"
    );
    verify_created_request(&runtime, &request).await?;
    claim_request_id(&runtime.state, &request.request_id).await?;
    let Ok(_approval_permit) = runtime.approvals.clone().try_acquire_owned() else {
        let decision = request.decision(
            AttendedGatewayDecisionKind::Deny,
            Some("approval_capacity_exhausted".into()),
        );
        record_remote_decision(
            &runtime,
            &request,
            LedgerStatus::RemoteDeny,
            protocol::AttendedGatewayTransitionKind::RemoteDeny,
        )
        .await?;
        write_frame(&mut stream, &decision).await?;
        return Ok(());
    };
    let (disconnect_tx, disconnect_rx) = watch::channel(false);
    let relay = relay_request(runtime.clone(), request.clone(), disconnect_rx);
    tokio::pin!(relay);
    let mut byte = [0u8; 1];
    let decision = tokio::select! {
        result = &mut relay => match result {
            Ok(decision) => Some(decision),
            Err(RelayFailure::RemoteUnsettled) => {
                anyhow::bail!("remote approval could not be settled")
            }
            Err(class) => Some(request.decision(
                AttendedGatewayDecisionKind::Deny,
                Some(class.as_str().into()),
            )),
        },
        read = stream.read(&mut byte) => {
            let _ = disconnect_tx.send(true);
            let _ = read;
            tokio::time::timeout(CANCELLATION_DRAIN_TIMEOUT, &mut relay)
                .await
                .context("disconnected approval cancellation timed out")?
                .map_err(|failure| anyhow::anyhow!(failure.as_str()))?;
            None
        }
    };
    if let Some(decision) = decision {
        ensure!(
            Utc::now() < request.deadline,
            "approval decision arrived after request deadline"
        );
        write_frame(&mut stream, &decision).await?;
    }
    Ok(())
}

async fn record_sandbox_transition(
    runtime: &RelayRuntime,
    transition: &protocol::AttendedGatewayTransitionV1,
) -> anyhow::Result<()> {
    use protocol::AttendedGatewayTransitionKind as Kind;

    ensure!(
        !matches!(
            transition.transition,
            Kind::RemoteAllow | Kind::RemoteDeny | Kind::RemoteCancel
        ),
        "sandbox cannot report an authoritative remote decision"
    );
    let mut state = runtime.state.lock().await;
    match transition.transition {
        Kind::PluginReady => {
            if state.ready {
                return Ok(());
            }
            state.ready = true;
        }
        Kind::TranscriptOnly => {
            ensure!(state.ready, "tool transition arrived before host readiness");
            let call_id = transition.call_id.as_deref().expect("validated");
            ensure!(
                !state.calls.contains_key(call_id),
                "duplicate or conflicting tool call ID"
            );
            state
                .calls
                .insert(call_id.to_string(), transition_call(transition, true));
        }
        Kind::Created => {
            ensure!(state.ready, "tool transition arrived before host readiness");
            let request_id = transition.request_id.as_deref().expect("validated");
            ensure!(
                !state.entries.contains_key(request_id),
                "duplicate or replayed execution request ID"
            );
            let call_id = transition.call_id.as_deref().expect("validated");
            let call = transition_call(transition, false);
            if let Some(existing) = state.calls.get(call_id) {
                ensure!(existing == &call, "conflicting tool call binding");
            } else {
                state.calls.insert(call_id.to_string(), call.clone());
            }
            let canonical = transition
                .canonical_args_digest
                .as_deref()
                .expect("validated");
            ensure!(
                !state.entries.values().any(|entry| {
                    entry.call == call && entry.canonical_args_digest == canonical
                }),
                "duplicate canonical execution binding"
            );
            ensure!(
                state.entries.len() < MAX_REPLAY_IDS,
                "host supervision call limit exceeded"
            );
            state.entries.insert(
                request_id.to_string(),
                LedgerEntry {
                    call,
                    canonical_args_digest: canonical.to_string(),
                    status: LedgerStatus::Created,
                },
            );
        }
        Kind::LocalAllow | Kind::LocalDeny | Kind::Completed | Kind::Failed => {
            let request_id = transition.request_id.as_deref().expect("validated");
            let entry = state
                .entries
                .get_mut(request_id)
                .context("execution transition omitted its created entry")?;
            verify_transition_binding(entry, transition)?;
            let next = match transition.transition {
                Kind::LocalAllow if entry.status == LedgerStatus::Created => {
                    LedgerStatus::LocalAllow
                }
                Kind::LocalDeny if entry.status == LedgerStatus::Created => LedgerStatus::LocalDeny,
                Kind::Completed
                    if matches!(
                        entry.status,
                        LedgerStatus::LocalAllow | LedgerStatus::RemoteAllow
                    ) =>
                {
                    LedgerStatus::Completed
                }
                Kind::Failed
                    if matches!(
                        entry.status,
                        LedgerStatus::LocalAllow | LedgerStatus::RemoteAllow
                    ) =>
                {
                    LedgerStatus::Failed
                }
                _ => anyhow::bail!("illegal, conflicting, or replayed supervision transition"),
            };
            entry.status = next;
        }
        Kind::RemoteAllow | Kind::RemoteDeny | Kind::RemoteCancel => unreachable!(),
    }
    append_ledger(&mut state, transition)?;
    Ok(())
}

fn transition_call(
    transition: &protocol::AttendedGatewayTransitionV1,
    transcript_only: bool,
) -> CallBinding {
    CallBinding {
        call_id: transition.call_id.clone().expect("validated"),
        session_id: transition.session_id.clone().expect("validated"),
        tool_name: transition.tool_name.clone().expect("validated"),
        raw_args_digest: transition.raw_args_digest.clone().expect("validated"),
        transcript_only,
    }
}

fn verify_transition_binding(
    entry: &LedgerEntry,
    transition: &protocol::AttendedGatewayTransitionV1,
) -> anyhow::Result<()> {
    ensure!(
        entry.call == transition_call(transition, false)
            && entry.canonical_args_digest
                == transition
                    .canonical_args_digest
                    .as_deref()
                    .expect("validated"),
        "execution transition binding or digest conflict"
    );
    Ok(())
}

fn append_ledger(
    state: &mut RelayState,
    transition: &protocol::AttendedGatewayTransitionV1,
) -> anyhow::Result<()> {
    ensure!(
        state.transitions < MAX_LEDGER_TRANSITIONS,
        "host supervision transition limit exceeded"
    );
    let mut bytes = serde_json::to_vec(transition)?;
    bytes.push(b'\n');
    ensure!(
        state.ledger_bytes.saturating_add(bytes.len()) <= MAX_LEDGER_BYTES,
        "host supervision ledger byte limit exceeded"
    );
    state.ledger_file.write_all(&bytes)?;
    state.ledger_file.sync_data()?;
    state.transitions += 1;
    state.ledger_bytes += bytes.len();
    Ok(())
}

async fn verify_created_request(
    runtime: &RelayRuntime,
    request: &AttendedGatewayRequestV1,
) -> anyhow::Result<()> {
    let state = runtime.state.lock().await;
    ensure!(state.ready, "approval arrived before host plugin readiness");
    let entry = state
        .entries
        .get(&request.request_id)
        .context("approval has no created host ledger entry")?;
    ensure!(
        entry.status == LedgerStatus::Created
            && entry.call.call_id_matches(request)
            && entry.canonical_args_digest == request.canonical_args_digest
            && entry.call.raw_args_digest == request.raw_args_digest,
        "approval conflicts with its host ledger entry"
    );
    Ok(())
}

impl CallBinding {
    fn call_id_matches(&self, request: &AttendedGatewayRequestV1) -> bool {
        self.call_id == request.call_id
            && self.session_id == request.approval.session_id.to_string()
            && self.tool_name == request.approval.tool.to_string()
    }
}

async fn claim_request_id(state: &Mutex<RelayState>, request_id: &str) -> anyhow::Result<()> {
    let mut state = state.lock().await;
    ensure!(
        state.request_ids.len() < MAX_REPLAY_IDS,
        "attended gateway replay state exhausted"
    );
    ensure!(
        state.request_ids.insert(request_id.to_string()),
        "replayed attended gateway request ID"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum RelayFailure {
    SocketIntegrity,
    Registration,
    Persistence,
    Wait,
    Deadline,
    Cancelled,
    RemoteUnsettled,
}

impl RelayFailure {
    fn as_str(self) -> &'static str {
        match self {
            Self::SocketIntegrity => "approval_socket_integrity_failed",
            Self::Registration => "approval_registration_failed",
            Self::Persistence => "approval_persistence_failed",
            Self::Wait => "approval_wait_failed",
            Self::Deadline => "approval_deadline_expired",
            Self::Cancelled => "approval_cancelled",
            Self::RemoteUnsettled => "approval_remote_unsettled",
        }
    }
}

async fn relay_request(
    runtime: RelayRuntime,
    mut request: AttendedGatewayRequestV1,
    mut disconnected: watch::Receiver<bool>,
) -> Result<protocol::AttendedGatewayDecisionV1, RelayFailure> {
    verify_realized_socket_mount(&runtime)
        .await
        .map_err(|_| RelayFailure::SocketIntegrity)?;
    request.approval.context.attended = Some(Box::new(protocol::AttendedApprovalReviewContext {
        run_epoch: request.binding.run_epoch.clone(),
        authorization_digest: request.binding.authorization_digest.clone(),
        environment_bundle_digest: request.binding.environment_bundle_digest.clone(),
        image_digest: request.binding.image_digest.clone(),
        network_digest: request.binding.network_digest.clone(),
        approval_bundle_digest: request.binding.approval_bundle_digest.clone(),
        approval_bundle: runtime.config.approval_bundle.clone(),
    }));
    let registration_resource = serde_json::to_string(&RegistrationEffect {
        request_id: request.request_id.clone(),
        request: request.clone(),
    })
    .map_err(|_| RelayFailure::Persistence)?;
    let register_effect = runtime
        .effects
        .begin_external_operation("attended_relay_approval_register", &registration_resource)
        .map_err(|_| RelayFailure::Persistence)?;
    let response: ApprovalResponse = send_json(
        authenticated(
            runtime.config.token.as_ref(),
            runtime.client.post(
                endpoint_url(&runtime.endpoint, "api/v1/hooks/approval")
                    .map_err(|_| RelayFailure::Registration)?,
            ),
        )
        .json(&request.approval),
        StatusCode::OK,
    )
    .await
    .map_err(|_| RelayFailure::Registration)?;
    runtime.state.lock().await.pending.insert(
        request.request_id.clone(),
        PendingApproval {
            approval_id: response.id,
        },
    );
    let result = async {
        persist_approval_state(
            &runtime.effects,
            &request.request_id,
            response.id,
            &response.status,
        )
        .map_err(|_| RelayFailure::Persistence)?;
        register_effect
            .completed()
            .map_err(|_| RelayFailure::Persistence)?;
        let mut status = response.status;
        while status == ApprovalStatus::Pending {
            if *disconnected.borrow() {
                status = cancel_remote(
                    &runtime.client,
                    &runtime.endpoint,
                    runtime.config.token.as_ref(),
                    response.id,
                    &runtime.effects,
                    &request.request_id,
                )
                .await
                .map_err(|_| RelayFailure::Cancelled)?;
                break;
            }
            let remaining = (request.deadline - Utc::now())
                .to_std()
                .map_err(|_| RelayFailure::Deadline)?;
            let wait = wait_once(&runtime, response.id, remaining.min(WAIT_SLICE));
            tokio::select! {
                result = wait => status = result.map_err(|_| RelayFailure::Wait)?,
                changed = disconnected.changed() => {
                    let _ = changed;
                    continue;
                }
            }
        }
        persist_approval_state(&runtime.effects, &request.request_id, response.id, &status)
            .map_err(|_| RelayFailure::Persistence)?;
        verify_terminal(
            &runtime.client,
            &runtime.endpoint,
            runtime.config.token.as_ref(),
            response.id,
            &request.request_id,
        )
        .await
        .map_err(|_| RelayFailure::Wait)?;
        reconcile_request_effects(&runtime.effects, &request.request_id)
            .map_err(|_| RelayFailure::Persistence)?;
        Ok::<_, RelayFailure>(status)
    }
    .await;
    let status = match result {
        Ok(status) => status,
        Err(failure) => {
            settle_remote_after_failure(&runtime, response.id, &request.request_id)
                .await
                .map_err(|_| RelayFailure::RemoteUnsettled)?;
            runtime
                .state
                .lock()
                .await
                .pending
                .remove(&request.request_id);
            return Err(failure);
        }
    };
    runtime
        .state
        .lock()
        .await
        .pending
        .remove(&request.request_id);
    let (ledger_status, transition) = match &status {
        ApprovalStatus::Approved { .. } => (
            LedgerStatus::RemoteAllow,
            protocol::AttendedGatewayTransitionKind::RemoteAllow,
        ),
        ApprovalStatus::Denied { .. } => (
            LedgerStatus::RemoteDeny,
            protocol::AttendedGatewayTransitionKind::RemoteDeny,
        ),
        ApprovalStatus::Cancelled | ApprovalStatus::Pending => (
            LedgerStatus::RemoteCancel,
            protocol::AttendedGatewayTransitionKind::RemoteCancel,
        ),
    };
    record_remote_decision(&runtime, &request, ledger_status, transition)
        .await
        .map_err(|_| RelayFailure::Persistence)?;
    Ok(match status {
        ApprovalStatus::Approved { message } => {
            request.decision(AttendedGatewayDecisionKind::Allow, message)
        }
        ApprovalStatus::Denied { reason } => {
            request.decision(AttendedGatewayDecisionKind::Deny, Some(reason))
        }
        ApprovalStatus::Cancelled | ApprovalStatus::Pending => request.decision(
            AttendedGatewayDecisionKind::Deny,
            Some(RelayFailure::Cancelled.as_str().into()),
        ),
    })
}

async fn record_remote_decision(
    runtime: &RelayRuntime,
    request: &AttendedGatewayRequestV1,
    status: LedgerStatus,
    transition: protocol::AttendedGatewayTransitionKind,
) -> anyhow::Result<()> {
    let mut state = runtime.state.lock().await;
    let entry = state
        .entries
        .get_mut(&request.request_id)
        .context("remote decision has no created host ledger entry")?;
    ensure!(
        entry.status == LedgerStatus::Created,
        "conflicting or replayed remote decision"
    );
    entry.status = status;
    let event = protocol::AttendedGatewayTransitionV1 {
        version: protocol::ATTENDED_GATEWAY_TRANSITION_VERSION.into(),
        binding: request.binding.clone(),
        transition,
        request_id: Some(request.request_id.clone()),
        call_id: Some(request.call_id.clone()),
        session_id: Some(request.approval.session_id.to_string()),
        tool_name: Some(request.approval.tool.to_string()),
        raw_args_digest: Some(request.raw_args_digest.clone()),
        canonical_args_digest: Some(request.canonical_args_digest.clone()),
        plugin_nonce: None,
    };
    append_ledger(&mut state, &event)
}

async fn settle_remote_after_failure(
    runtime: &RelayRuntime,
    approval_id: Uuid,
    request_id: &str,
) -> anyhow::Result<()> {
    let approval = get_approval(
        &runtime.client,
        &runtime.endpoint,
        runtime.config.token.as_ref(),
        approval_id,
    )
    .await?;
    verify_remote_identity(&approval, approval_id, request_id)?;
    if approval.status == ApprovalStatus::Pending {
        cancel_remote(
            &runtime.client,
            &runtime.endpoint,
            runtime.config.token.as_ref(),
            approval_id,
            &runtime.effects,
            request_id,
        )
        .await?;
    }
    verify_terminal(
        &runtime.client,
        &runtime.endpoint,
        runtime.config.token.as_ref(),
        approval_id,
        request_id,
    )
    .await?;
    reconcile_request_effects(&runtime.effects, request_id)
}

async fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> anyhow::Result<T> {
    let mut header = [0u8; 4];
    tokio::time::timeout(FRAME_READ_TIMEOUT, stream.read_exact(&mut header))
        .await
        .context("attended gateway frame header timed out")??;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=ATTENDED_GATEWAY_MAX_FRAME_BYTES).contains(&length),
        "attended gateway frame length is out of bounds"
    );
    let mut payload = vec![0; length];
    tokio::time::timeout(FRAME_READ_TIMEOUT, stream.read_exact(&mut payload))
        .await
        .context("attended gateway frame payload timed out")??;
    decode_attended_gateway_payload(&payload).map_err(anyhow::Error::msg)
}

async fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> anyhow::Result<()> {
    let frame = encode_attended_gateway_frame(value).map_err(anyhow::Error::msg)?;
    tokio::time::timeout(FRAME_READ_TIMEOUT, stream.write_all(&frame))
        .await
        .context("attended gateway response write timed out")??;
    stream.shutdown().await?;
    Ok(())
}

async fn wait_once(
    runtime: &RelayRuntime,
    approval_id: Uuid,
    timeout: Duration,
) -> anyhow::Result<ApprovalStatus> {
    let response: ApprovalWaitResponse = send_json(
        authenticated(
            runtime.config.token.as_ref(),
            runtime.client.get(endpoint_url(
                &runtime.endpoint,
                &format!("api/v1/approvals/{approval_id}/wait"),
            )?),
        )
        .timeout(timeout),
        StatusCode::OK,
    )
    .await?;
    Ok(response.status)
}

async fn cancel_all_pending(runtime: &RelayRuntime) -> anyhow::Result<()> {
    let pending = std::mem::take(&mut runtime.state.lock().await.pending);
    let mut failures = Vec::new();
    for (request_id, pending) in pending {
        let result = async {
            let status = cancel_remote(
                &runtime.client,
                &runtime.endpoint,
                runtime.config.token.as_ref(),
                pending.approval_id,
                &runtime.effects,
                &request_id,
            )
            .await?;
            ensure!(status.is_resolved(), "cancelled approval is not terminal");
            verify_terminal(
                &runtime.client,
                &runtime.endpoint,
                runtime.config.token.as_ref(),
                pending.approval_id,
                &request_id,
            )
            .await?;
            reconcile_request_effects(&runtime.effects, &request_id)
        }
        .await;
        if let Err(error) = result {
            failures.push(format!("{request_id}: {error:#}"));
        }
    }
    ensure!(
        failures.is_empty(),
        "pending approval cancellation failed: {}",
        failures.join("; ")
    );
    Ok(())
}

async fn cancel_remote(
    client: &Client,
    endpoint: &Url,
    token: Option<&Secret>,
    approval_id: Uuid,
    effects: &ExecutionCancellationContext,
    request_id: &str,
) -> anyhow::Result<ApprovalStatus> {
    let operation = effects.begin_external_operation(
        "attended_relay_approval_cancel",
        &serde_json::to_string(&serde_json::json!({
            "request_id": request_id,
            "approval_id": approval_id,
        }))?,
    )?;
    let approval: Approval = send_json(
        authenticated(
            token,
            client.post(endpoint_url(
                endpoint,
                &format!("api/v1/approvals/{approval_id}/resolve"),
            )?),
        )
        .json(&ApprovalResolveRequest {
            decision: ApprovalDecision::Cancel,
            message: None,
        }),
        StatusCode::OK,
    )
    .await?;
    verify_remote_identity(&approval, approval_id, request_id)?;
    operation.completed()?;
    persist_approval_state(effects, request_id, approval_id, &approval.status)?;
    Ok(approval.status)
}

async fn verify_terminal(
    client: &Client,
    endpoint: &Url,
    token: Option<&Secret>,
    approval_id: Uuid,
    request_id: &str,
) -> anyhow::Result<ApprovalStatus> {
    let approval = get_approval(client, endpoint, token, approval_id).await?;
    verify_remote_identity(&approval, approval_id, request_id)?;
    ensure!(approval.status.is_resolved(), "approval remains pending");
    Ok(approval.status)
}

fn verify_remote_identity(
    approval: &Approval,
    approval_id: Uuid,
    request_id: &str,
) -> anyhow::Result<()> {
    ensure!(
        approval.id == approval_id && approval.request_id == request_id,
        "approval server returned mismatched approval identity"
    );
    Ok(())
}

async fn get_approval(
    client: &Client,
    endpoint: &Url,
    token: Option<&Secret>,
    approval_id: Uuid,
) -> anyhow::Result<Approval> {
    send_json(
        authenticated(
            token,
            client.get(endpoint_url(
                endpoint,
                &format!("api/v1/approvals/{approval_id}"),
            )?),
        ),
        StatusCode::OK,
    )
    .await
}

fn persist_approval_state(
    effects: &ExecutionCancellationContext,
    request_id: &str,
    approval_id: Uuid,
    status: &ApprovalStatus,
) -> anyhow::Result<()> {
    let resource = serde_json::to_string(&ApprovalStateEffect {
        request_id: request_id.to_string(),
        approval_id,
        status: status.clone(),
    })?;
    effects
        .begin_external_operation("attended_relay_approval_state", &resource)?
        .completed()
}

fn reconcile_request_effects(
    effects: &ExecutionCancellationContext,
    request_id: &str,
) -> anyhow::Result<()> {
    for kind in [
        "attended_relay_approval_register",
        "attended_relay_approval_state",
        "attended_relay_approval_cancel",
    ] {
        effects.reconcile_operation_kind_resource(kind, request_id)?;
    }
    Ok(())
}

async fn send_json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    expected: StatusCode,
) -> anyhow::Result<T> {
    let mut response = request.send().await?;
    ensure!(
        response.status() == expected
            || (expected == StatusCode::OK && response.status() == StatusCode::ACCEPTED),
        "approval server returned classified HTTP failure"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= ATTENDED_GATEWAY_MAX_FRAME_BYTES,
            "approval server response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("approval server returned invalid response")
}

async fn verify_realized_socket_mount(runtime: &RelayRuntime) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DockerInspectMount {
        #[serde(rename = "Type")]
        mount_type: String,
        source: String,
        destination: String,
        #[serde(rename = "RW")]
        read_write: bool,
    }
    let config = &runtime.config;
    verify_socket(&config.socket_path, Some(runtime.socket_identity))?;
    let parent = config
        .socket_path
        .parent()
        .context("approval relay socket has no parent")?;
    let output = tokio::time::timeout(
        HTTP_OPERATION_TIMEOUT,
        tokio::process::Command::new(&config.docker_path)
            .args([
                "inspect",
                "--format",
                "{{json .Mounts}}",
                &config.container_name,
            ])
            .output(),
    )
    .await
    .context("inspect attended relay bind mount timed out")??;
    ensure!(
        output.status.success(),
        "cannot verify attended relay bind mount"
    );
    ensure!(
        output.stdout.len() <= ATTENDED_GATEWAY_MAX_FRAME_BYTES,
        "Docker mount inspection exceeded limit"
    );
    let mounts: Vec<DockerInspectMount> = serde_json::from_slice(&output.stdout)?;
    let matching = mounts
        .iter()
        .filter(|mount| mount.destination == "/agent-hub/approval")
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1,
        "running container has no one exact approval parent mount"
    );
    let mount = matching[0];
    ensure!(
        mount.mount_type == "bind" && Path::new(&mount.source) == parent && !mount.read_write,
        "running container approval parent mount type, source, or mode mismatch"
    );
    Ok(())
}

fn authenticated(
    token: Option<&Secret>,
    request: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token.expose()),
        None => request,
    }
}

fn endpoint_url(endpoint: &Url, path: &str) -> anyhow::Result<Url> {
    let mut url = endpoint.clone();
    let base = endpoint.path().trim_end_matches('/');
    url.set_path(&format!("{base}/{path}"));
    Ok(url)
}

pub(crate) fn validated_endpoint(value: &str, has_token: bool) -> anyhow::Result<Url> {
    let url = Url::parse(value).context("approval server URL is invalid")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "approval server URL must use HTTP or HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "approval server URL must not contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "approval server URL must not contain query or fragment"
    );
    let host = url.host_str().context("approval server URL has no host")?;
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    if url.scheme() == "http" {
        ensure!(
            loopback,
            "HTTP approval endpoints require a literal loopback address"
        );
    } else if !loopback {
        ensure!(
            has_token,
            "non-loopback approval endpoints require HTTPS plus a bearer token"
        );
    }
    Ok(url)
}

fn remove_socket(
    path: &Path,
    identity: SocketIdentity,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    verify_socket(path, Some(identity))?;
    let remove = effects
        .begin_external_operation("attended_relay_socket_remove", &path.to_string_lossy())?;
    std::fs::remove_file(path)?;
    remove.completed()?;
    ensure!(
        std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "approval relay socket remains after cleanup"
    );
    for kind in [
        "attended_relay_socket_bind",
        "attended_relay_socket_chmod",
        "attended_relay_socket_remove",
    ] {
        effects.reconcile_operation_kind_resource(kind, &path.to_string_lossy())?;
    }
    Ok(())
}

fn remove_socket_if_exact(
    path: &Path,
    identity: SocketIdentity,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => remove_socket(path, identity, effects),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for kind in [
                "attended_relay_socket_bind",
                "attended_relay_socket_chmod",
                "attended_relay_socket_remove",
            ] {
                effects.reconcile_operation_kind_resource(kind, &path.to_string_lossy())?;
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn prepare_socket_root(
    socket_root: &Path,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<DirectoryIdentity> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let runtime_parent = socket_root
        .parent()
        .context("approval relay socket root has no Agent Hub parent")?;
    let runtime_dir = runtime_parent
        .parent()
        .context("approval relay Agent Hub directory has no runtime parent")?;
    verify_runtime_directory(runtime_dir, unsafe { libc::geteuid() })?;
    ensure!(
        runtime_parent
            .file_name()
            .is_some_and(|name| name == "agent-hub"),
        "approval relay socket root is outside the Agent Hub runtime directory"
    );
    if std::fs::symlink_metadata(runtime_parent)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        let create = effects.begin_external_operation(
            "attended_relay_runtime_parent_create",
            &runtime_parent.to_string_lossy(),
        )?;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(runtime_parent) {
            Ok(()) => create.completed()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                create.reconciled()?
            }
            Err(error) => {
                if std::fs::symlink_metadata(runtime_parent)
                    .is_err_and(|inspect| inspect.kind() == std::io::ErrorKind::NotFound)
                {
                    create.reconciled()?;
                }
                return Err(error).context("create Agent Hub runtime directory");
            }
        }
    }
    verify_private_directory(runtime_parent, None)?;
    effects.reconcile_operation_kind_resource(
        "attended_relay_runtime_parent_create",
        &runtime_parent.to_string_lossy(),
    )?;
    ensure!(
        std::fs::symlink_metadata(socket_root)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "approval relay socket root already exists"
    );
    let create = effects.begin_external_operation(
        "attended_relay_socket_root_create",
        &socket_root.to_string_lossy(),
    )?;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    if let Err(error) = builder.create(socket_root) {
        if std::fs::symlink_metadata(socket_root)
            .is_err_and(|inspect| inspect.kind() == std::io::ErrorKind::NotFound)
        {
            create.reconciled()?;
        }
        return Err(error).context("create attended relay socket root");
    }
    std::fs::set_permissions(socket_root, std::fs::Permissions::from_mode(0o700))?;
    let identity = verify_private_directory(socket_root, None)?;
    create.completed()?;
    Ok(identity)
}

#[cfg(not(unix))]
fn prepare_socket_root(
    _socket_root: &Path,
    _effects: &ExecutionCancellationContext,
) -> anyhow::Result<DirectoryIdentity> {
    anyhow::bail!("attended approval relay requires Unix directories")
}

fn remove_socket_root(
    socket_root: &Path,
    expected: Option<DirectoryIdentity>,
    effects: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(socket_root) {
        Ok(_) => {
            verify_private_directory(socket_root, expected)?;
            let remove = effects.begin_external_operation(
                "attended_relay_socket_root_remove",
                &socket_root.to_string_lossy(),
            )?;
            std::fs::remove_dir(socket_root)?;
            remove.completed()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    ensure!(
        std::fs::symlink_metadata(socket_root)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "approval relay socket root remains after cleanup"
    );
    for kind in [
        "attended_relay_socket_root_create",
        "attended_relay_socket_root_remove",
        "attended_relay_socket_root_recovery_remove",
    ] {
        effects.reconcile_operation_kind_resource(kind, &socket_root.to_string_lossy())?;
    }
    Ok(())
}

#[cfg(unix)]
fn verify_private_directory(
    path: &Path,
    expected: Option<DirectoryIdentity>,
) -> anyhow::Result<DirectoryIdentity> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "approval relay runtime path is not a real directory"
    );
    ensure!(
        std::fs::canonicalize(path)? == path,
        "approval relay runtime path is not canonical"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "approval relay runtime path owner mismatch"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o700,
        "approval relay runtime path mode is not 0700"
    );
    let identity = DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    };
    if let Some(expected) = expected {
        ensure!(
            identity == expected,
            "approval relay socket root inode changed"
        );
    }
    Ok(identity)
}

#[cfg(not(unix))]
fn verify_private_directory(
    _path: &Path,
    _expected: Option<DirectoryIdentity>,
) -> anyhow::Result<DirectoryIdentity> {
    anyhow::bail!("attended approval relay requires Unix directories")
}

#[cfg(unix)]
fn verify_socket_parent(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let parent = path
        .parent()
        .context("approval relay socket has no parent")?;
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "approval relay parent is not a real directory"
    );
    ensure!(
        std::fs::canonicalize(parent)? == parent,
        "approval relay parent is not canonical"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "approval relay parent owner mismatch"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o700,
        "approval relay parent mode is not 0700"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_socket_parent(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("attended approval relay requires Unix sockets")
}

#[cfg(unix)]
fn verify_socket(path: &Path, expected: Option<SocketIdentity>) -> anyhow::Result<SocketIdentity> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    verify_socket_parent(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket(),
        "approval relay path is not a Unix socket"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "approval relay socket owner mismatch"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o600,
        "approval relay socket mode is not 0600"
    );
    let identity = SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    };
    if let Some(expected) = expected {
        ensure!(identity == expected, "approval relay socket inode changed");
    }
    Ok(identity)
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> anyhow::Result<SocketIdentity> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    verify_socket_parent(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket(),
        "approval relay path is not a Unix socket"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "approval relay socket owner mismatch"
    );
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    })
}

#[cfg(not(unix))]
fn socket_identity(_path: &Path) -> anyhow::Result<SocketIdentity> {
    anyhow::bail!("attended approval relay requires Unix sockets")
}

#[cfg(not(unix))]
fn verify_socket(
    _path: &Path,
    _expected: Option<SocketIdentity>,
) -> anyhow::Result<SocketIdentity> {
    anyhow::bail!("attended approval relay requires Unix sockets")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        ApprovalContext, ApprovalRequest, HookEventName, RequestType, SessionId, Tool,
        tool_input_digest,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join(format!("attended-relay-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        root
    }

    fn test_effects(root: &Path) -> ExecutionCancellationContext {
        ExecutionCancellationContext::new(
            &root.join("effects"),
            agent_hub_executor::Cancellation::never(),
        )
        .unwrap()
    }

    fn ledger_runtime(root: &Path) -> RelayRuntime {
        let request = gateway_request();
        let approval_bundle = protocol::AttendedApprovalBundleIdentity {
            name: "approval".into(),
            digest: request.binding.approval_bundle_digest.clone(),
            protocol: protocol::APPROVAL_BUNDLE_PROTOCOL.into(),
            manifest_sha256: "a".repeat(64),
            gateway_commit: "gateway".into(),
            gateway_archive_sha256: "b".repeat(64),
            plugin_sha256: "c".repeat(64),
            dippy_commit: "dippy".into(),
            dippy_archive_sha256: "d".repeat(64),
            dippy_wheel_sha256: "e".repeat(64),
        };
        RelayRuntime {
            config: AttendedRelayConfig {
                socket_path: root.join("unused.sock"),
                binding: request.binding,
                approval_bundle,
                run_deadline: Utc::now() + chrono::Duration::minutes(1),
                server_url: "http://127.0.0.1:1".into(),
                token: None,
                docker_path: PathBuf::from("docker"),
                container_name: "unused".into(),
                ledger_path: root.join("ledger.jsonl"),
                plugin_nonce: "f".repeat(32),
            },
            endpoint: Url::parse("http://127.0.0.1:1").unwrap(),
            client: Client::new(),
            state: Arc::new(Mutex::new(RelayState {
                request_ids: HashSet::new(),
                pending: HashMap::new(),
                ready: false,
                calls: HashMap::new(),
                entries: HashMap::new(),
                transitions: 0,
                ledger_bytes: 0,
                ledger_file: create_private_ledger(&root.join("ledger.jsonl")).unwrap(),
            })),
            effects: test_effects(root),
            approvals: Arc::new(Semaphore::new(MAX_IN_FLIGHT_APPROVALS)),
            socket_identity: SocketIdentity {
                device: 0,
                inode: 0,
                uid: unsafe { libc::geteuid() },
            },
        }
    }

    fn transition(
        runtime: &RelayRuntime,
        kind: protocol::AttendedGatewayTransitionKind,
    ) -> protocol::AttendedGatewayTransitionV1 {
        protocol::AttendedGatewayTransitionV1 {
            version: protocol::ATTENDED_GATEWAY_TRANSITION_VERSION.into(),
            binding: runtime.config.binding.clone(),
            transition: kind,
            request_id: Some("018f0f21-76c0-7b37-a446-31f82a5e7602".into()),
            call_id: Some("call-1".into()),
            session_id: Some("attended-test".into()),
            tool_name: Some("Bash".into()),
            raw_args_digest: Some("a".repeat(64)),
            canonical_args_digest: Some("b".repeat(64)),
            plugin_nonce: None,
        }
    }

    #[tokio::test]
    async fn host_ledger_enforces_readiness_monotonicity_and_completion() {
        let root = test_root("ledger-monotonicity");
        let runtime = ledger_runtime(&root);
        let created = transition(&runtime, protocol::AttendedGatewayTransitionKind::Created);
        assert!(record_sandbox_transition(&runtime, &created).await.is_err());
        let mut ready = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::PluginReady,
        );
        ready.request_id = None;
        ready.call_id = None;
        ready.session_id = None;
        ready.tool_name = None;
        ready.raw_args_digest = None;
        ready.canonical_args_digest = None;
        ready.plugin_nonce = Some("f".repeat(32));
        record_sandbox_transition(&runtime, &ready).await.unwrap();
        record_sandbox_transition(&runtime, &ready).await.unwrap();
        record_sandbox_transition(&runtime, &created).await.unwrap();
        assert!(record_sandbox_transition(&runtime, &created).await.is_err());
        let allowed = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::LocalAllow,
        );
        record_sandbox_transition(&runtime, &allowed).await.unwrap();
        assert!(record_sandbox_transition(&runtime, &allowed).await.is_err());
        assert!(runtime_outcome(&runtime, false).await.is_err());
        let completed = transition(&runtime, protocol::AttendedGatewayTransitionKind::Completed);
        record_sandbox_transition(&runtime, &completed)
            .await
            .unwrap();
        assert_eq!(
            runtime_outcome(&runtime, false).await.unwrap(),
            SupervisionOutcome::NoDenials
        );
        runtime.effects.finalize().unwrap();
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn runtime_outcome(
        runtime: &RelayRuntime,
        cancelled: bool,
    ) -> anyhow::Result<SupervisionOutcome> {
        let state = runtime.state.lock().await;
        supervision_outcome(&state, cancelled)
    }

    #[tokio::test]
    async fn cancellation_is_distinct_without_overriding_host_denials() {
        let root = test_root("ledger-cancellation");
        let runtime = ledger_runtime(&root);
        assert_eq!(
            runtime_outcome(&runtime, true).await.unwrap(),
            SupervisionOutcome::Cancelled
        );
        let mut ready = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::PluginReady,
        );
        ready.request_id = None;
        ready.call_id = None;
        ready.session_id = None;
        ready.tool_name = None;
        ready.raw_args_digest = None;
        ready.canonical_args_digest = None;
        ready.plugin_nonce = Some("f".repeat(32));
        record_sandbox_transition(&runtime, &ready).await.unwrap();
        record_sandbox_transition(
            &runtime,
            &transition(&runtime, protocol::AttendedGatewayTransitionKind::Created),
        )
        .await
        .unwrap();
        assert_eq!(
            runtime_outcome(&runtime, true).await.unwrap(),
            SupervisionOutcome::Cancelled
        );
        record_sandbox_transition(
            &runtime,
            &transition(&runtime, protocol::AttendedGatewayTransitionKind::LocalDeny),
        )
        .await
        .unwrap();
        assert_eq!(
            runtime_outcome(&runtime, true).await.unwrap(),
            SupervisionOutcome::Denied
        );
        runtime.effects.finalize().unwrap();
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn host_ledger_rejects_forged_remote_and_conflicting_digests() {
        let root = test_root("ledger-authority");
        let runtime = ledger_runtime(&root);
        let mut ready = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::PluginReady,
        );
        ready.request_id = None;
        ready.call_id = None;
        ready.session_id = None;
        ready.tool_name = None;
        ready.raw_args_digest = None;
        ready.canonical_args_digest = None;
        ready.plugin_nonce = Some("f".repeat(32));
        record_sandbox_transition(&runtime, &ready).await.unwrap();
        let created = transition(&runtime, protocol::AttendedGatewayTransitionKind::Created);
        record_sandbox_transition(&runtime, &created).await.unwrap();
        let forged = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::RemoteAllow,
        );
        assert!(record_sandbox_transition(&runtime, &forged).await.is_err());
        let mut conflict = transition(
            &runtime,
            protocol::AttendedGatewayTransitionKind::LocalAllow,
        );
        conflict.canonical_args_digest = Some("c".repeat(64));
        assert!(
            record_sandbox_transition(&runtime, &conflict)
                .await
                .is_err()
        );
        record_sandbox_transition(
            &runtime,
            &transition(&runtime, protocol::AttendedGatewayTransitionKind::LocalDeny),
        )
        .await
        .unwrap();
        assert_eq!(
            runtime_outcome(&runtime, false).await.unwrap(),
            SupervisionOutcome::Denied
        );
        runtime.effects.finalize().unwrap();
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_ledger_rejects_symlinks_replacement_and_hard_limits() {
        let root = test_root("ledger-bounds");
        let target = root.join("target");
        std::fs::write(&target, b"forged sandbox evidence").unwrap();
        let link = root.join("ledger-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(create_private_ledger(&link).is_err());

        let runtime = ledger_runtime(&root);
        let event = transition(&runtime, protocol::AttendedGatewayTransitionKind::Created);
        {
            let mut state = runtime.state.blocking_lock();
            state.transitions = MAX_LEDGER_TRANSITIONS;
            assert!(append_ledger(&mut state, &event).is_err());
            state.transitions = 0;
            state.ledger_bytes = MAX_LEDGER_BYTES;
            assert!(append_ledger(&mut state, &event).is_err());
        }
        runtime.effects.finalize().unwrap();
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn relay_location_is_short_even_when_attended_state_root_is_long() {
        use std::os::unix::ffi::OsStrExt as _;

        let long_state_root = PathBuf::from("/state").join("x".repeat(4096));
        let location = host_relay_socket_location(Uuid::new_v4()).unwrap();
        assert!(!location.socket_path.starts_with(long_state_root));
        assert!(location.socket_path.as_os_str().as_bytes().len() < UNIX_SOCKET_PATH_CAPACITY);
        let too_long = PathBuf::from("/").join("x".repeat(UNIX_SOCKET_PATH_CAPACITY));
        assert!(verify_socket_path_length(&too_long).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_directory_rejects_symlink_wrong_owner_and_non_runtime_filesystem() {
        let uid = unsafe { libc::geteuid() };
        let runtime = PathBuf::from(format!("/run/user/{uid}"));
        verify_runtime_directory(&runtime, uid).unwrap();
        assert!(verify_runtime_directory(&runtime, uid.wrapping_add(1)).is_err());

        let root = test_root("runtime-validation");
        let link = root.join("runtime-link");
        std::os::unix::fs::symlink(&runtime, &link).unwrap();
        assert!(verify_runtime_directory(&link, uid).is_err());
        assert!(verify_runtime_directory(&root, uid).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bind_address_in_use_preserves_existing_socket_and_reconciles_bind() {
        let journal_root = test_root("address-in-use");
        let effects = test_effects(&journal_root);
        let location = host_relay_socket_location(Uuid::new_v4()).unwrap();
        prepare_socket_root(&location.socket_root, &effects).unwrap();
        let socket = location.socket_path;
        let existing = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let error = bind_listener(&socket, &effects).unwrap_err();
        assert!(format!("{error:#}").contains("Address already in use"));
        assert!(socket_identity(&socket).is_ok());
        drop(existing);
        std::fs::remove_file(&socket).unwrap();
        remove_socket_root(&location.socket_root, None, &effects).unwrap();
        effects.finalize().unwrap();
        std::fs::remove_dir_all(journal_root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn crash_recovery_removes_run_owned_socket_root_idempotently() {
        use std::os::unix::fs::PermissionsExt as _;

        let journal_root = test_root("crash-recovery");
        let effects = test_effects(&journal_root);
        let location = host_relay_socket_location(Uuid::new_v4()).unwrap();
        prepare_socket_root(&location.socket_root, &effects).unwrap();
        let listener = bind_listener(&location.socket_path, &effects).unwrap();
        std::fs::set_permissions(
            &location.socket_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        effects
            .begin_external_operation(
                "attended_relay_socket_chmod",
                &location.socket_path.to_string_lossy(),
            )
            .unwrap()
            .completed()
            .unwrap();
        drop(listener);

        recover_attended_relay_socket(&location.socket_path, &effects).unwrap();
        recover_attended_relay_socket(&location.socket_path, &effects).unwrap();
        assert!(!location.socket_path.exists());
        assert!(!location.socket_root.exists());
        effects.finalize().unwrap();
        std::fs::remove_dir_all(journal_root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_unix_socket_starts_listens_and_cleans_up() {
        let journal_root = test_root("real-socket");
        let effects = test_effects(&journal_root);
        let location = host_relay_socket_location(Uuid::new_v4()).unwrap();
        let binding = gateway_request().binding;
        let approval_bundle = protocol::AttendedApprovalBundleIdentity {
            name: "approval".into(),
            digest: binding.approval_bundle_digest.clone(),
            protocol: protocol::APPROVAL_BUNDLE_PROTOCOL.into(),
            manifest_sha256: "a".repeat(64),
            gateway_commit: "gateway".into(),
            gateway_archive_sha256: "b".repeat(64),
            plugin_sha256: "c".repeat(64),
            dippy_commit: "dippy".into(),
            dippy_archive_sha256: "d".repeat(64),
            dippy_wheel_sha256: "e".repeat(64),
        };
        let (fail_closed, _) = watch::channel(false);
        let mut relay = AttendedRelay::start(
            AttendedRelayConfig {
                socket_path: location.socket_path.clone(),
                binding,
                approval_bundle,
                run_deadline: Utc::now() + chrono::Duration::minutes(1),
                server_url: "http://127.0.0.1:1".into(),
                token: None,
                docker_path: PathBuf::from("docker"),
                container_name: "unused".into(),
                ledger_path: journal_root.join("supervision.jsonl"),
                plugin_nonce: "f".repeat(32),
            },
            &effects,
            fail_closed,
        )
        .await
        .unwrap();
        let _stream = UnixStream::connect(&location.socket_path).await.unwrap();
        relay.stop().await.unwrap();
        effects.finalize().unwrap();
        assert!(!location.socket_root.exists());
        std::fs::remove_dir_all(journal_root).unwrap();
    }

    #[test]
    fn endpoint_rejects_credentials_and_insecure_bearer() {
        assert!(validated_endpoint("https://token@example.test", false).is_err());
        assert!(validated_endpoint("file:///server", false).is_err());
        assert!(validated_endpoint("http://example.test", true).is_err());
        assert!(validated_endpoint("http://example.test", false).is_err());
        assert!(validated_endpoint("https://example.test", false).is_err());
        validated_endpoint("https://example.test/approvals", true).unwrap();
        validated_endpoint("http://127.0.0.1:8080", true).unwrap();
        validated_endpoint("http://127.0.0.1:8080", false).unwrap();
    }

    #[tokio::test]
    async fn recovery_replays_registration_and_cancellation_fault_boundaries() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-attended-relay-recovery-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let effects = ExecutionCancellationContext::new(
            &root.join("effects"),
            agent_hub_executor::Cancellation::never(),
        )
        .unwrap();
        let request = gateway_request();
        let request_id = request.request_id.clone();
        let resource = serde_json::to_string(&RegistrationEffect {
            request_id: request.request_id.clone(),
            request,
        })
        .unwrap();
        effects
            .begin_external_operation("attended_relay_approval_register", &resource)
            .unwrap()
            .completed()
            .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let registration_attempts = Arc::new(AtomicUsize::new(0));
        let cancellation_attempts = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(fake_approval_server(
            listener,
            Arc::clone(&registration_attempts),
            Arc::clone(&cancellation_attempts),
            request_id,
        ));
        let socket = root.join("approval.sock");

        assert!(
            recover_attended_relay(&socket, &endpoint, None, &effects)
                .await
                .is_err()
        );
        assert!(
            recover_attended_relay(&socket, &endpoint, None, &effects)
                .await
                .is_err()
        );
        recover_attended_relay(&socket, &endpoint, None, &effects)
            .await
            .unwrap();
        effects.finalize().unwrap();
        assert_eq!(registration_attempts.load(Ordering::SeqCst), 3);
        assert_eq!(cancellation_attempts.load(Ordering::SeqCst), 2);
        assert!(!socket.exists());

        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn replay_state_allows_concurrency_and_hard_bounds_unique_ids() {
        let root = test_root("replay-state");
        let state = Arc::new(Mutex::new(RelayState {
            request_ids: HashSet::new(),
            pending: HashMap::new(),
            ready: false,
            calls: HashMap::new(),
            entries: HashMap::new(),
            transitions: 0,
            ledger_bytes: 0,
            ledger_file: create_private_ledger(&root.join("ledger.jsonl")).unwrap(),
        }));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..64 {
            let state = Arc::clone(&state);
            tasks.spawn(async move { claim_request_id(&state, &Uuid::new_v4().to_string()).await });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let duplicate = Uuid::new_v4().to_string();
        claim_request_id(&state, &duplicate).await.unwrap();
        assert!(claim_request_id(&state, &duplicate).await.is_err());
        while state.lock().await.request_ids.len() < MAX_REPLAY_IDS {
            claim_request_id(&state, &Uuid::new_v4().to_string())
                .await
                .unwrap();
        }
        assert!(
            claim_request_id(&state, &Uuid::new_v4().to_string())
                .await
                .is_err()
        );
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn gateway_request() -> AttendedGatewayRequestV1 {
        let request_id = Uuid::new_v4().to_string();
        let tool_input = serde_json::json!({"command": "true"});
        AttendedGatewayRequestV1 {
            version: protocol::ATTENDED_GATEWAY_REQUEST_VERSION.into(),
            binding: AttendedGatewayBindingV1 {
                run_epoch: Uuid::new_v4().to_string(),
                authorization_digest: "a".repeat(64),
                environment_bundle_digest: "b".repeat(64),
                image_digest: format!("sha256:{}", "c".repeat(64)),
                network_digest: "d".repeat(64),
                approval_bundle_digest: "e".repeat(64),
            },
            request_id: request_id.clone(),
            call_id: "call-1".into(),
            raw_args_digest: protocol::opencode_raw_args_digest(&tool_input).unwrap(),
            canonical_args_digest: protocol::opencode_match_input_digest(&tool_input).unwrap(),
            approval: ApprovalRequest {
                id: request_id,
                request_digest: None,
                session_id: SessionId::new("attended-test"),
                session_display_name: "Attended test".into(),
                cwd: "/workspace/main".into(),
                tool: Tool::Bash,
                tool_input: tool_input.clone(),
                provider: "opencode".into(),
                request_type: RequestType::ToolUse,
                context: ApprovalContext {
                    workspace_roots: vec!["/workspace/main".into()],
                    hook_event_name: HookEventName::from("permission.ask".to_string()),
                    extra: None,
                    attended: None,
                },
            },
            tool_input_digest: tool_input_digest(&tool_input).unwrap(),
            deadline: Utc::now() + chrono::Duration::minutes(1),
        }
    }

    async fn fake_approval_server(
        listener: tokio::net::TcpListener,
        registration_attempts: Arc<AtomicUsize>,
        cancellation_attempts: Arc<AtomicUsize>,
        request_id: String,
    ) {
        let approval_id = Uuid::new_v4();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let registration_attempts = Arc::clone(&registration_attempts);
            let cancellation_attempts = Arc::clone(&cancellation_attempts);
            let request_id = request_id.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    request.push(byte[0]);
                    if request.len() > 16 * 1024 {
                        return;
                    }
                }
                let line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if line.starts_with("POST /api/v1/hooks/approval ")
                    && registration_attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return;
                }
                if line.contains("/resolve ")
                    && cancellation_attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return;
                }
                let body = if line.starts_with("POST /api/v1/hooks/approval ") {
                    serde_json::to_vec(&ApprovalResponse {
                        id: approval_id,
                        status: ApprovalStatus::Pending,
                    })
                    .unwrap()
                } else {
                    serde_json::to_vec(&Approval {
                        id: approval_id,
                        request_id,
                        session_id: SessionId::new("attended-test"),
                        session_display_name: "Attended test".into(),
                        project: "main".into(),
                        tool: Tool::Bash,
                        tool_input: serde_json::json!({"command": "true"}),
                        provider: "opencode".into(),
                        request_type: RequestType::ToolUse,
                        context: ApprovalContext {
                            workspace_roots: vec!["/workspace/main".into()],
                            hook_event_name: HookEventName::from("permission.ask".to_string()),
                            extra: None,
                            attended: None,
                        },
                        created_at: Utc::now(),
                        status: ApprovalStatus::Cancelled,
                    })
                    .unwrap()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    }
}

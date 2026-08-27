use std::collections::{HashMap, HashSet};
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
const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_SLICE: Duration = Duration::from_secs(60);
const CANCELLATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(crate) struct AttendedRelayConfig {
    pub socket_path: PathBuf,
    pub binding: AttendedGatewayBindingV1,
    pub run_deadline: DateTime<Utc>,
    pub server_url: String,
    pub token: Option<Secret>,
    pub docker_path: PathBuf,
    pub container_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
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
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    runtime: RelayRuntime,
}

impl AttendedRelay {
    pub(crate) async fn start(
        config: AttendedRelayConfig,
        effects: &ExecutionCancellationContext,
        fail_closed: watch::Sender<bool>,
    ) -> anyhow::Result<Self> {
        let endpoint = validated_endpoint(&config.server_url, config.token.is_some())?;
        ensure!(
            config.socket_path.is_absolute(),
            "approval relay socket path is not absolute"
        );
        verify_socket_parent(&config.socket_path)?;
        ensure!(
            std::fs::symlink_metadata(&config.socket_path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "approval relay socket path already exists"
        );
        let bind = effects.begin_external_operation(
            "attended_relay_socket_bind",
            &config.socket_path.to_string_lossy(),
        )?;
        let listener = UnixListener::bind(&config.socket_path).with_context(|| {
            format!(
                "bind approval relay socket {}",
                config.socket_path.display()
            )
        })?;
        bind.completed()?;
        let chmod = effects.begin_external_operation(
            "attended_relay_socket_chmod",
            &config.socket_path.to_string_lossy(),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        chmod.completed()?;
        let identity = verify_socket(&config.socket_path, None)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(HTTP_OPERATION_TIMEOUT)
            .timeout(HTTP_OPERATION_TIMEOUT)
            .build()?;
        let runtime = RelayRuntime {
            config: config.clone(),
            endpoint,
            client,
            state: Arc::new(Mutex::new(RelayState {
                request_ids: HashSet::new(),
                pending: HashMap::new(),
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
        let socket_result = remove_socket(&self.socket_path, self.identity, &self.runtime.effects);
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
    match std::fs::symlink_metadata(socket_path) {
        Ok(_) => {
            verify_socket(socket_path, None)?;
            let remove = effects.begin_external_operation(
                "attended_relay_socket_recovery_remove",
                &socket_path.to_string_lossy(),
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
        effects.reconcile_operation_kind_resource(kind, &socket_path.to_string_lossy())?;
    }
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
    let request: AttendedGatewayRequestV1 = read_frame(&mut stream).await?;
    request
        .validate(&runtime.config.binding, Utc::now())
        .map_err(anyhow::Error::msg)?;
    ensure!(
        request.deadline <= runtime.config.run_deadline,
        "approval deadline exceeds run deadline"
    );
    claim_request_id(&runtime.state, &request.request_id).await?;
    let Ok(_approval_permit) = runtime.approvals.clone().try_acquire_owned() else {
        let decision = request.decision(
            AttendedGatewayDecisionKind::Deny,
            Some("approval_capacity_exhausted".into()),
        );
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
    request: AttendedGatewayRequestV1,
    mut disconnected: watch::Receiver<bool>,
) -> Result<protocol::AttendedGatewayDecisionV1, RelayFailure> {
    verify_realized_socket_mount(&runtime)
        .await
        .map_err(|_| RelayFailure::SocketIntegrity)?;
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
    if has_token && url.scheme() == "http" {
        let host = url.host_str().context("approval server URL has no host")?;
        let loopback = host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
        ensure!(
            loopback,
            "bearer authentication requires HTTPS or a literal loopback HTTP address"
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

    #[test]
    fn endpoint_rejects_credentials_and_insecure_bearer() {
        assert!(validated_endpoint("https://token@example.test", false).is_err());
        assert!(validated_endpoint("file:///server", false).is_err());
        assert!(validated_endpoint("http://example.test", true).is_err());
        validated_endpoint("https://example.test/approvals", true).unwrap();
        validated_endpoint("http://127.0.0.1:8080", true).unwrap();
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
        let state = Arc::new(Mutex::new(RelayState {
            request_ids: HashSet::new(),
            pending: HashMap::new(),
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
            approval: ApprovalRequest {
                id: request_id,
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

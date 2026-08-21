use anyhow::Context;
use protocol::{
    AuthenticatedWorkerSession, HeartbeatRequest, Job, JobResult, JobState, JobUpdateRequest,
    PollJobRequest, PollJobResponse, RegisterWorkerRequest, RegisterWorkerResponse, Secret,
    WorkerState,
};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use local_agent::pb;
use local_agent::pb_conv::{
    execution_mode_to_pb, job_from_pb, job_result_to_pb, job_state_to_pb, worker_state_to_pb,
};

const WORKER_NOT_FOUND_MARKER: &str = "worker not found";

#[derive(Clone)]
pub(crate) enum Client {
    Http(HttpClient),
    Grpc(GrpcClient),
}

#[derive(Clone)]
pub(crate) struct HttpClient {
    pub(crate) http: reqwest::Client,
    pub(crate) base: String,
    pub(crate) token: Option<Secret>,
    pub(crate) worker_session: Arc<RwLock<Option<AuthenticatedWorkerSession>>>,
}

#[derive(Clone)]
pub(crate) struct GrpcClient {
    pub(crate) client: pb::worker_lifecycle_client::WorkerLifecycleClient<Channel>,
    pub(crate) token: Option<Secret>,
    pub(crate) worker_session: Arc<RwLock<Option<AuthenticatedWorkerSession>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobUpdateOutcome {
    Applied,
    CancelledOnServer,
}

#[derive(Debug)]
pub(crate) struct WorkerNotFoundError;

impl std::fmt::Display for WorkerNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("worker not found on server")
    }
}

impl std::error::Error for WorkerNotFoundError {}

pub(crate) fn is_worker_not_found_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<WorkerNotFoundError>().is_some()
}

impl HttpClient {
    pub(crate) fn new(base: String, token: Option<Secret>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            token,
            worker_session: Arc::new(RwLock::new(None)),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.bearer_auth(token.expose())
        } else {
            req
        }
    }

    fn worker_session(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let session = self
            .worker_session
            .read()
            .expect("worker session lock poisoned")
            .clone();
        if let Some(session) = session {
            req.header(
                "x-agent-hub-worker-session-id",
                session.session_id.to_string(),
            )
            .header(
                "x-agent-hub-worker-session-token",
                session.session_token.expose(),
            )
        } else {
            req
        }
    }
}

impl Client {
    pub(crate) fn worker_session(&self) -> Option<AuthenticatedWorkerSession> {
        match self {
            Client::Http(client) => client
                .worker_session
                .read()
                .expect("worker session lock poisoned")
                .clone(),
            Client::Grpc(client) => client
                .worker_session
                .read()
                .expect("worker session lock poisoned")
                .clone(),
        }
    }

    pub(crate) async fn register_worker(&self, req: &RegisterWorkerRequest) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/workers", client.base);
                let resp = client
                    .worker_session(client.auth(client.http.post(url)))
                    .json(req)
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("register failed: {}", resp.status());
                }
                let registered: RegisterWorkerResponse = resp.json().await?;
                *client
                    .worker_session
                    .write()
                    .expect("worker session lock poisoned") = Some(AuthenticatedWorkerSession {
                    worker_id: req.id.clone(),
                    session_id: registered.session_id,
                    session_token: registered.session_token,
                    generation: registered.session_generation,
                });
                Ok(())
            }
            Client::Grpc(client) => {
                let worker_id = req.id.clone();
                let mut inner = client.client.clone();
                let mut req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::RegisterWorkerRequest {
                        worker_id: req.id.clone(),
                        hostname: req.hostname.clone(),
                        supported_modes: req
                            .supported_modes
                            .iter()
                            .cloned()
                            .map(execution_mode_to_pb)
                            .collect(),
                        protocol_capabilities: req
                            .protocol_capabilities
                            .iter()
                            .map(|capability| match capability {
                                protocol::WorkerProtocolCapability::CanonicalAuthorizationV1 => {
                                    pb::WorkerProtocolCapability::CanonicalAuthorizationV1 as i32
                                }
                            })
                            .collect(),
                    }),
                )?;
                apply_worker_session_metadata(
                    &mut req,
                    client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .as_ref(),
                )?;
                let registered = inner.register_worker(req).await?.into_inner();
                let session_id = uuid::Uuid::parse_str(&registered.session_id)
                    .context("invalid worker session ID from server")?;
                *client
                    .worker_session
                    .write()
                    .expect("worker session lock poisoned") = Some(AuthenticatedWorkerSession {
                    worker_id,
                    session_id,
                    session_token: Secret::new(registered.session_token),
                    generation: registered.session_generation,
                });
                Ok(())
            }
        }
    }

    pub(crate) async fn heartbeat(&self, id: &str, state: WorkerState) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!(
                    "{}/api/v1/orchestration/workers/{id}/heartbeat",
                    client.base
                );
                let resp = client
                    .worker_session(client.auth(client.http.post(url)))
                    .json(&HeartbeatRequest { state })
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    if resp.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(WorkerNotFoundError.into());
                    }
                    anyhow::bail!("heartbeat failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let mut req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::HeartbeatWorkerRequest {
                        worker_id: id.to_string(),
                        state: worker_state_to_pb(state),
                    }),
                )?;
                apply_worker_session_metadata(
                    &mut req,
                    client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .as_ref(),
                )?;
                match inner.heartbeat_worker(req).await {
                    Ok(_) => {}
                    Err(status)
                        if status.code() == tonic::Code::NotFound
                            && status.message().contains(WORKER_NOT_FOUND_MARKER) =>
                    {
                        return Err(WorkerNotFoundError.into());
                    }
                    Err(status) => return Err(status.into()),
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn poll(&self, worker_id: &str) -> anyhow::Result<Option<Job>> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/poll", client.base);
                let resp = client
                    .worker_session(client.auth(client.http.post(url)))
                    .json(&PollJobRequest {
                        worker_id: worker_id.to_string(),
                    })
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    if resp.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(WorkerNotFoundError.into());
                    }
                    anyhow::bail!("poll failed: {}", resp.status());
                }
                let parsed: PollJobResponse = resp.json().await?;
                if parsed
                    .job
                    .as_ref()
                    .is_some_and(|job| job.canonical_authorization.is_none())
                {
                    anyhow::bail!(
                        "job is incompatible: missing canonical authorization bytes; recreate it with a current server"
                    );
                }
                if let Some(job) = parsed.job.as_ref() {
                    let session = client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .clone()
                        .context("worker has no authenticated session")?;
                    if session.worker_id != worker_id {
                        anyhow::bail!("authenticated worker session identity mismatch");
                    }
                    local_agent::verify_authorized_job(
                        job,
                        worker_id,
                        session.session_id,
                        session.generation,
                    )
                    .map_err(anyhow::Error::msg)?;
                }
                Ok(parsed.job)
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let mut req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::PollJobRequest {
                        worker_id: worker_id.to_string(),
                    }),
                )?;
                apply_worker_session_metadata(
                    &mut req,
                    client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .as_ref(),
                )?;
                let resp = match inner.poll_job(req).await {
                    Ok(resp) => resp,
                    Err(status)
                        if status.code() == tonic::Code::NotFound
                            && status.message().contains(WORKER_NOT_FOUND_MARKER) =>
                    {
                        return Err(WorkerNotFoundError.into());
                    }
                    Err(status) => return Err(status.into()),
                }
                .into_inner();
                let job = resp.job.map(job_from_pb).transpose()?;
                if let Some(job) = job.as_ref() {
                    let session = client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .clone()
                        .context("worker has no authenticated session")?;
                    if session.worker_id != worker_id {
                        anyhow::bail!("authenticated worker session identity mismatch");
                    }
                    local_agent::verify_authorized_job(
                        job,
                        worker_id,
                        session.session_id,
                        session.generation,
                    )
                    .map_err(anyhow::Error::msg)?;
                }
                Ok(job)
            }
        }
    }

    pub(crate) async fn update_job(
        &self,
        worker_id: &str,
        job_id: uuid::Uuid,
        state: JobState,
        transcript_dir: Option<String>,
        result: Option<JobResult>,
    ) -> anyhow::Result<JobUpdateOutcome> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/jobs/{job_id}", client.base);
                let resp = client
                    .worker_session(client.auth(client.http.put(url)))
                    .json(&JobUpdateRequest {
                        worker_id: worker_id.to_string(),
                        state,
                        transcript_dir,
                        result,
                    })
                    .send()
                    .await?;
                let status = resp.status();
                if status.is_success() {
                    return Ok(JobUpdateOutcome::Applied);
                }
                if status == reqwest::StatusCode::CONFLICT {
                    let body = resp.text().await.unwrap_or_default();
                    if body.contains("job already cancelled") {
                        return Ok(JobUpdateOutcome::CancelledOnServer);
                    }
                }
                anyhow::bail!("update job failed: {status}");
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let mut req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::UpdateJobRequest {
                        worker_id: worker_id.to_string(),
                        job_id: job_id.to_string(),
                        state: job_state_to_pb(state),
                        transcript_dir,
                        result: result.map(job_result_to_pb),
                    }),
                )?;
                apply_worker_session_metadata(
                    &mut req,
                    client
                        .worker_session
                        .read()
                        .expect("worker session lock poisoned")
                        .as_ref(),
                )?;
                match inner.update_job(req).await {
                    Ok(_) => Ok(JobUpdateOutcome::Applied),
                    Err(status)
                        if status.code() == tonic::Code::FailedPrecondition
                            && status.message().contains("job already cancelled") =>
                    {
                        Ok(JobUpdateOutcome::CancelledOnServer)
                    }
                    Err(status) => Err(status.into()),
                }
            }
        }
    }

    pub(crate) async fn dispatch(&self) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/dispatch", client.base);
                let resp = client.auth(client.http.post(url)).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("dispatch failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(_) => Ok(()),
        }
    }
}

pub(crate) async fn start_grpc_event_stream(
    mut client: pb::worker_lifecycle_client::WorkerLifecycleClient<Channel>,
    token: Option<Secret>,
    worker_id: &str,
    worker_session: &AuthenticatedWorkerSession,
) -> anyhow::Result<mpsc::Sender<pb::JobEventChunk>> {
    let (tx, rx) = mpsc::channel::<pb::JobEventChunk>(512);
    let mut req = apply_grpc_bearer(token.as_ref(), tonic::Request::new(ReceiverStream::new(rx)))?;
    apply_worker_session_metadata(&mut req, Some(worker_session))?;
    req.metadata_mut().insert(
        "x-agent-hub-worker-id",
        MetadataValue::try_from(worker_id).context("invalid worker ID for gRPC metadata")?,
    );
    tokio::spawn(async move {
        if let Err(error) = client.stream_job_events(req).await {
            eprintln!("gRPC event stream ended: {error}");
        }
    });
    Ok(tx)
}

fn apply_worker_session_metadata<T>(
    request: &mut tonic::Request<T>,
    credential: Option<&AuthenticatedWorkerSession>,
) -> anyhow::Result<()> {
    if let Some(credential) = credential {
        request.metadata_mut().insert(
            "x-agent-hub-worker-session-id",
            MetadataValue::try_from(credential.session_id.to_string())
                .context("invalid worker session ID for gRPC metadata")?,
        );
        request.metadata_mut().insert(
            "x-agent-hub-worker-session-token",
            MetadataValue::try_from(credential.session_token.expose())
                .context("invalid worker session token for gRPC metadata")?,
        );
    }
    Ok(())
}

fn apply_grpc_bearer<T>(
    token: Option<&Secret>,
    mut request: tonic::Request<T>,
) -> anyhow::Result<tonic::Request<T>> {
    if let Some(token) = token {
        let value = MetadataValue::try_from(format!("Bearer {}", token.expose()))
            .context("invalid AGENT_HUB_TOKEN for gRPC metadata")?;
        request.metadata_mut().insert("authorization", value);
    }
    Ok(request)
}

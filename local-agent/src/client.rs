use anyhow::Context;
use protocol::{
    HeartbeatRequest, Job, JobResult, JobState, JobUpdateRequest, PollJobRequest, PollJobResponse,
    RegisterWorkerRequest, Secret, WorkerState,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::pb;
use crate::pb_conv::{
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
}

#[derive(Clone)]
pub(crate) struct GrpcClient {
    pub(crate) client: pb::worker_lifecycle_client::WorkerLifecycleClient<Channel>,
    pub(crate) token: Option<Secret>,
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
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.bearer_auth(token.expose())
        } else {
            req
        }
    }
}

impl Client {
    pub(crate) async fn register_worker(&self, req: &RegisterWorkerRequest) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/workers", client.base);
                let resp = client.auth(client.http.post(url)).json(req).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("register failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
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
                    }),
                )?;
                inner.register_worker(req).await?;
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
                    .auth(client.http.post(url))
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
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::HeartbeatWorkerRequest {
                        worker_id: id.to_string(),
                        state: worker_state_to_pb(state),
                    }),
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
                    .auth(client.http.post(url))
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
                Ok(parsed.job)
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::PollJobRequest {
                        worker_id: worker_id.to_string(),
                    }),
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
                Ok(resp.job.map(job_from_pb).transpose()?)
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
                    .auth(client.http.put(url))
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
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::UpdateJobRequest {
                        worker_id: worker_id.to_string(),
                        job_id: job_id.to_string(),
                        state: job_state_to_pb(state),
                        transcript_dir,
                        result: result.map(job_result_to_pb),
                    }),
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
) -> anyhow::Result<mpsc::Sender<pb::JobEventChunk>> {
    let (tx, rx) = mpsc::channel::<pb::JobEventChunk>(512);
    let req = apply_grpc_bearer(token.as_ref(), tonic::Request::new(ReceiverStream::new(rx)))?;
    tokio::spawn(async move {
        if let Err(error) = client.stream_job_events(req).await {
            eprintln!("gRPC event stream ended: {error}");
        }
    });
    Ok(tx)
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

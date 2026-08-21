#![allow(clippy::result_large_err)]

use chrono::{DateTime, Utc};
use protocol::{
    ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming, GithubPullRequestMetadata,
    HeartbeatRequest, Job, JobEvent, JobEventKind, JobResult, JobState, JobUpdateRequest,
    PollJobRequest, PromptRef, RegisterWorkerRequest, RepoExecutionMetadata, RepoSource,
    StructuredTranscriptArtifact, TerminationReason, WorkerProtocolCapability,
    WorkerSessionCredential, WorkerState,
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::error::AppError;
use crate::server::auth::match_bearer_token;
use crate::server::config::{AuthMode, ServerConfig};
use crate::server::job_events::JobEventHub;
use crate::server::orchestration::OrchestrationService;

#[allow(clippy::enum_variant_names)]
pub mod pb {
    tonic::include_proto!("worker");
}

pub struct WorkerLifecycleGrpc {
    orchestration: std::sync::Arc<OrchestrationService>,
    job_events: Arc<JobEventHub>,
}

const WORKER_SESSION_ID_METADATA: &str = "x-agent-hub-worker-session-id";
const WORKER_SESSION_TOKEN_METADATA: &str = "x-agent-hub-worker-session-token";
const WORKER_ID_METADATA: &str = "x-agent-hub-worker-id";

fn worker_session_from_metadata<T>(
    request: &Request<T>,
    required: bool,
) -> Result<Option<WorkerSessionCredential>, Status> {
    let session_id = request.metadata().get(WORKER_SESSION_ID_METADATA);
    let session_token = request.metadata().get(WORKER_SESSION_TOKEN_METADATA);
    match (session_id, session_token) {
        (None, None) if !required => Ok(None),
        (Some(session_id), Some(session_token)) => {
            let session_id = session_id
                .to_str()
                .ok()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| Status::unauthenticated("invalid worker session ID"))?;
            let session_token = session_token
                .to_str()
                .map_err(|_| Status::unauthenticated("invalid worker session token"))?;
            if session_token.is_empty() {
                return Err(Status::unauthenticated("invalid worker session token"));
            }
            Ok(Some(WorkerSessionCredential {
                session_id,
                session_token: protocol::Secret::new(session_token),
            }))
        }
        _ => Err(Status::unauthenticated(
            "incomplete worker session credential",
        )),
    }
}

impl WorkerLifecycleGrpc {
    pub fn new(
        orchestration: std::sync::Arc<OrchestrationService>,
        job_events: Arc<JobEventHub>,
    ) -> Self {
        Self {
            orchestration,
            job_events,
        }
    }
}

fn map_orchestration_error(error: AppError) -> Status {
    match error {
        AppError::JobCancelled(message) => Status::failed_precondition(message),
        AppError::WorkerSessionUnauthorized(message) => Status::unauthenticated(message),
        other => Status::internal(other.to_string()),
    }
}

#[derive(Clone)]
pub struct GrpcAuthInterceptor {
    config: Arc<ServerConfig>,
}

impl GrpcAuthInterceptor {
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self { config }
    }
}

impl tonic::service::Interceptor for GrpcAuthInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        if self.config.auth_mode == AuthMode::None {
            return Ok(request);
        }

        let header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("missing authorization metadata"))?;

        if match_bearer_token(header, &self.config.tokens).is_some() {
            Ok(request)
        } else {
            Err(Status::unauthenticated("invalid bearer token"))
        }
    }
}

#[tonic::async_trait]
impl pb::worker_lifecycle_server::WorkerLifecycle for WorkerLifecycleGrpc {
    async fn register_worker(
        &self,
        request: Request<pb::RegisterWorkerRequest>,
    ) -> Result<Response<pb::RegisterWorkerResponse>, Status> {
        let replacement = worker_session_from_metadata(&request, false)?;
        let req = request.into_inner();
        let supported_modes = req
            .supported_modes
            .iter()
            .copied()
            .map(execution_mode_from_pb)
            .collect::<Result<Vec<_>, _>>()?;
        let protocol_capabilities = req
            .protocol_capabilities
            .iter()
            .copied()
            .map(worker_protocol_capability_from_pb)
            .collect::<Result<Vec<_>, _>>()?;

        let registered = self
            .orchestration
            .register_worker_session(
                RegisterWorkerRequest {
                    id: req.worker_id,
                    hostname: req.hostname,
                    supported_modes,
                    protocol_capabilities,
                },
                replacement,
            )
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::RegisterWorkerResponse {
            session_id: registered.session_id.to_string(),
            session_token: registered.session_token.expose().to_string(),
            session_generation: registered.session_generation,
        }))
    }

    async fn heartbeat_worker(
        &self,
        request: Request<pb::HeartbeatWorkerRequest>,
    ) -> Result<Response<pb::HeartbeatWorkerResponse>, Status> {
        let credential = worker_session_from_metadata(&request, true)?.expect("required session");
        let req = request.into_inner();
        let state = worker_state_from_pb(req.state)?;
        self.orchestration
            .heartbeat_authenticated(req.worker_id, credential, HeartbeatRequest { state })
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::HeartbeatWorkerResponse {}))
    }

    async fn poll_job(
        &self,
        request: Request<pb::PollJobRequest>,
    ) -> Result<Response<pb::PollJobResponse>, Status> {
        let credential = worker_session_from_metadata(&request, true)?.expect("required session");
        let req = request.into_inner();
        let response = self
            .orchestration
            .poll_job_authenticated(
                credential,
                PollJobRequest {
                    worker_id: req.worker_id,
                },
            )
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::PollJobResponse {
            job: response.job.map(job_to_pb).transpose()?,
        }))
    }

    async fn update_job(
        &self,
        request: Request<pb::UpdateJobRequest>,
    ) -> Result<Response<pb::UpdateJobResponse>, Status> {
        let credential = worker_session_from_metadata(&request, true)?.expect("required session");
        let req = request.into_inner();
        let state = job_state_from_pb(req.state)?;
        let job_id = Uuid::parse_str(&req.job_id)
            .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;
        let transcript_dir = req.transcript_dir;
        let result = req.result.map(job_result_from_pb).transpose()?;

        self.orchestration
            .update_job_authenticated(
                job_id,
                credential,
                JobUpdateRequest {
                    worker_id: req.worker_id,
                    state,
                    transcript_dir,
                    result,
                },
            )
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::UpdateJobResponse {}))
    }

    async fn stream_job_events(
        &self,
        request: Request<tonic::Streaming<pb::JobEventChunk>>,
    ) -> Result<Response<pb::StreamJobEventsResponse>, Status> {
        let credential = worker_session_from_metadata(&request, true)?.expect("required session");
        let worker_id = request
            .metadata()
            .get(WORKER_ID_METADATA)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Status::unauthenticated("missing worker identity metadata"))?
            .to_string();
        self.orchestration
            .verify_worker_session(&worker_id, &credential)
            .await
            .map_err(map_orchestration_error)?;
        let mut stream = request.into_inner();
        let mut accepted: u32 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Status::internal(e.to_string()))?;
            if chunk.worker_id != worker_id {
                return Err(Status::unauthenticated(
                    "event worker identity does not match session",
                ));
            }
            let job_id = Uuid::parse_str(&chunk.job_id)
                .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;

            let kind = job_event_kind_from_pb(chunk.kind)?;
            let at = chunk
                .at
                .map(ts_from_pb)
                .transpose()?
                .unwrap_or_else(Utc::now);

            let event = JobEvent {
                job_id,
                worker_id: chunk.worker_id,
                at,
                kind,
                stream: chunk.stream,
                message: chunk.message,
            };
            self.orchestration
                .record_worker_event_authenticated(credential.clone(), &event)
                .await
                .map_err(map_orchestration_error)?;
            self.job_events.push(event).await;
            accepted = accepted.saturating_add(1);
        }

        Ok(Response::new(pb::StreamJobEventsResponse { accepted }))
    }
}

fn execution_mode_to_pb(mode: ExecutionMode) -> i32 {
    match mode {
        ExecutionMode::Docker => pb::ExecutionMode::Docker as i32,
    }
}

#[allow(dead_code)]
fn job_event_kind_to_pb(v: JobEventKind) -> i32 {
    match v {
        JobEventKind::System => pb::JobEventKind::JobEventSystem as i32,
        JobEventKind::Stdout => pb::JobEventKind::JobEventStdout as i32,
        JobEventKind::Stderr => pb::JobEventKind::JobEventStderr as i32,
    }
}

fn job_event_kind_from_pb(v: i32) -> Result<JobEventKind, Status> {
    match pb::JobEventKind::try_from(v).unwrap_or(pb::JobEventKind::JobEventUnspecified) {
        pb::JobEventKind::JobEventSystem => Ok(JobEventKind::System),
        pb::JobEventKind::JobEventStdout => Ok(JobEventKind::Stdout),
        pb::JobEventKind::JobEventStderr => Ok(JobEventKind::Stderr),
        pb::JobEventKind::JobEventUnspecified => {
            Err(Status::invalid_argument("job_event_kind unspecified"))
        }
    }
}

fn execution_mode_from_pb(value: i32) -> Result<ExecutionMode, Status> {
    match pb::ExecutionMode::try_from(value).unwrap_or(pb::ExecutionMode::Unspecified) {
        pb::ExecutionMode::Docker => Ok(ExecutionMode::Docker),
        pb::ExecutionMode::Unspecified => {
            Err(Status::invalid_argument("execution_mode unspecified"))
        }
    }
}

fn job_state_to_pb(state: JobState) -> i32 {
    match state {
        JobState::Pending => pb::JobState::Pending as i32,
        JobState::Assigned => pb::JobState::Assigned as i32,
        JobState::Running => pb::JobState::Running as i32,
        JobState::Succeeded => pb::JobState::Succeeded as i32,
        JobState::Failed => pb::JobState::Failed as i32,
        JobState::Cancelled => pb::JobState::Cancelled as i32,
    }
}

fn job_state_from_pb(value: i32) -> Result<JobState, Status> {
    match pb::JobState::try_from(value).unwrap_or(pb::JobState::JobUnspecified) {
        pb::JobState::Pending => Ok(JobState::Pending),
        pb::JobState::Assigned => Ok(JobState::Assigned),
        pb::JobState::Running => Ok(JobState::Running),
        pb::JobState::Succeeded => Ok(JobState::Succeeded),
        pb::JobState::Failed => Ok(JobState::Failed),
        pb::JobState::Cancelled => Ok(JobState::Cancelled),
        pb::JobState::JobUnspecified => Err(Status::invalid_argument("job_state unspecified")),
    }
}

fn worker_state_from_pb(value: i32) -> Result<WorkerState, Status> {
    match pb::WorkerState::try_from(value).unwrap_or(pb::WorkerState::WorkerUnspecified) {
        pb::WorkerState::Idle => Ok(WorkerState::Idle),
        pb::WorkerState::Busy => Ok(WorkerState::Busy),
        pb::WorkerState::Offline => Ok(WorkerState::Offline),
        pb::WorkerState::WorkerUnspecified => {
            Err(Status::invalid_argument("worker_state unspecified"))
        }
    }
}

fn worker_protocol_capability_from_pb(value: i32) -> Result<WorkerProtocolCapability, Status> {
    match pb::WorkerProtocolCapability::try_from(value)
        .unwrap_or(pb::WorkerProtocolCapability::Unspecified)
    {
        pb::WorkerProtocolCapability::CanonicalAuthorizationV1 => {
            Ok(WorkerProtocolCapability::CanonicalAuthorizationV1)
        }
        pb::WorkerProtocolCapability::Unspecified => Err(Status::invalid_argument(
            "worker protocol capability unspecified",
        )),
    }
}

fn execution_outcome_to_pb(v: ExecutionOutcome) -> i32 {
    match v {
        ExecutionOutcome::Succeeded => pb::ExecutionOutcome::OutcomeSucceeded as i32,
        ExecutionOutcome::Failed => pb::ExecutionOutcome::OutcomeFailed as i32,
        ExecutionOutcome::TimedOut => pb::ExecutionOutcome::OutcomeTimedOut as i32,
        ExecutionOutcome::Cancelled => pb::ExecutionOutcome::OutcomeCancelled as i32,
    }
}

fn execution_outcome_from_pb(v: i32) -> Result<ExecutionOutcome, Status> {
    match pb::ExecutionOutcome::try_from(v).unwrap_or(pb::ExecutionOutcome::OutcomeUnspecified) {
        pb::ExecutionOutcome::OutcomeSucceeded => Ok(ExecutionOutcome::Succeeded),
        pb::ExecutionOutcome::OutcomeFailed => Ok(ExecutionOutcome::Failed),
        pb::ExecutionOutcome::OutcomeTimedOut => Ok(ExecutionOutcome::TimedOut),
        pb::ExecutionOutcome::OutcomeCancelled => Ok(ExecutionOutcome::Cancelled),
        pb::ExecutionOutcome::OutcomeUnspecified => {
            Err(Status::invalid_argument("execution_outcome unspecified"))
        }
    }
}

fn termination_reason_to_pb(v: TerminationReason) -> i32 {
    match v {
        TerminationReason::ExitCode => pb::TerminationReason::ReasonExitCode as i32,
        TerminationReason::Timeout => pb::TerminationReason::ReasonTimeout as i32,
        TerminationReason::Cancelled => pb::TerminationReason::ReasonCancelled as i32,
        TerminationReason::WorkerError => pb::TerminationReason::ReasonWorkerError as i32,
    }
}

fn termination_reason_from_pb(v: i32) -> Result<TerminationReason, Status> {
    match pb::TerminationReason::try_from(v).unwrap_or(pb::TerminationReason::ReasonUnspecified) {
        pb::TerminationReason::ReasonExitCode => Ok(TerminationReason::ExitCode),
        pb::TerminationReason::ReasonTimeout => Ok(TerminationReason::Timeout),
        pb::TerminationReason::ReasonCancelled => Ok(TerminationReason::Cancelled),
        pb::TerminationReason::ReasonWorkerError => Ok(TerminationReason::WorkerError),
        pb::TerminationReason::ReasonUnspecified => {
            Err(Status::invalid_argument("termination_reason unspecified"))
        }
    }
}

fn ts_to_pb(v: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: v.timestamp(),
        nanos: v.timestamp_subsec_nanos() as i32,
    }
}

fn ts_from_pb(v: prost_types::Timestamp) -> Result<DateTime<Utc>, Status> {
    DateTime::from_timestamp(v.seconds, v.nanos as u32)
        .ok_or_else(|| Status::invalid_argument("invalid timestamp"))
}

fn json_to_pb(v: serde_json::Value) -> prost_types::Value {
    match v {
        serde_json::Value::Null => prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        },
        serde_json::Value::Bool(b) => prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(b)),
        },
        serde_json::Value::Number(n) => prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(
                n.as_f64().unwrap_or_default(),
            )),
        },
        serde_json::Value::String(s) => prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s)),
        },
        serde_json::Value::Array(arr) => prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(
                prost_types::ListValue {
                    values: arr.into_iter().map(json_to_pb).collect(),
                },
            )),
        },
        serde_json::Value::Object(obj) => prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                fields: obj.into_iter().map(|(k, v)| (k, json_to_pb(v))).collect(),
            })),
        },
    }
}

fn json_from_pb(v: prost_types::Value) -> Result<serde_json::Value, Status> {
    Ok(match v.kind {
        None => serde_json::Value::Null,
        Some(prost_types::value::Kind::NullValue(_)) => serde_json::Value::Null,
        Some(prost_types::value::Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(prost_types::value::Kind::NumberValue(n)) => serde_json::json!(n),
        Some(prost_types::value::Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(prost_types::value::Kind::ListValue(list)) => serde_json::Value::Array(
            list.values
                .into_iter()
                .map(json_from_pb)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(prost_types::value::Kind::StructValue(s)) => serde_json::Value::Object(
            s.fields
                .into_iter()
                .map(|(k, v)| Ok((k, json_from_pb(v)?)))
                .collect::<Result<serde_json::Map<_, _>, Status>>()?,
        ),
    })
}

fn prompt_to_pb(v: PromptRef) -> pb::PromptRef {
    pb::PromptRef {
        system: v.system,
        user: v.user,
    }
}

#[cfg(test)]
fn prompt_from_pb(v: pb::PromptRef) -> PromptRef {
    PromptRef {
        system: v.system,
        user: v.user,
    }
}

fn repo_to_pb(v: RepoSource) -> pb::RepoSource {
    pb::RepoSource {
        repo_url: v.repo_url,
        base_ref: v.base_ref,
        branch_name: v.branch_name,
    }
}

#[cfg(test)]
fn repo_from_pb(v: pb::RepoSource) -> RepoSource {
    RepoSource {
        repo_url: v.repo_url,
        base_ref: v.base_ref,
        branch_name: v.branch_name,
    }
}

fn artifact_to_pb(v: ArtifactSummary) -> pb::ArtifactSummary {
    pb::ArtifactSummary {
        stdout_path: v.stdout_path,
        stderr_path: v.stderr_path,
        output_path: v.output_path,
        transcript_dir: v.transcript_dir,
        stdout_bytes: v.stdout_bytes,
        stderr_bytes: v.stderr_bytes,
        output_bytes: v.output_bytes,
        structured_transcript_artifacts: v
            .structured_transcript_artifacts
            .into_iter()
            .map(structured_artifact_to_pb)
            .collect(),
    }
}

fn artifact_from_pb(v: pb::ArtifactSummary) -> ArtifactSummary {
    ArtifactSummary {
        stdout_path: v.stdout_path,
        stderr_path: v.stderr_path,
        output_path: v.output_path,
        transcript_dir: v.transcript_dir,
        stdout_bytes: v.stdout_bytes,
        stderr_bytes: v.stderr_bytes,
        output_bytes: v.output_bytes,
        structured_transcript_artifacts: v
            .structured_transcript_artifacts
            .into_iter()
            .map(structured_artifact_from_pb)
            .collect(),
    }
}

fn structured_artifact_to_pb(v: StructuredTranscriptArtifact) -> pb::StructuredTranscriptArtifact {
    pb::StructuredTranscriptArtifact {
        key: v.key,
        path: v.path,
        bytes: v.bytes,
        schema: v.schema,
        record_count: v.record_count,
        inline_json: v.inline_json.map(json_to_pb),
    }
}

fn structured_artifact_from_pb(
    v: pb::StructuredTranscriptArtifact,
) -> StructuredTranscriptArtifact {
    StructuredTranscriptArtifact {
        key: v.key,
        path: v.path,
        bytes: v.bytes,
        schema: v.schema,
        record_count: v.record_count,
        inline_json: v.inline_json.map(json_from_pb).transpose().ok().flatten(),
    }
}

fn timing_to_pb(v: ExecutionTiming) -> pb::ExecutionTiming {
    pb::ExecutionTiming {
        started_at: Some(ts_to_pb(v.started_at)),
        finished_at: Some(ts_to_pb(v.finished_at)),
        duration_ms: v.duration_ms,
    }
}

fn timing_from_pb(v: pb::ExecutionTiming) -> Result<ExecutionTiming, Status> {
    Ok(ExecutionTiming {
        started_at: ts_from_pb(
            v.started_at
                .ok_or_else(|| Status::invalid_argument("missing started_at"))?,
        )?,
        finished_at: ts_from_pb(
            v.finished_at
                .ok_or_else(|| Status::invalid_argument("missing finished_at"))?,
        )?,
        duration_ms: v.duration_ms,
    })
}

fn repo_meta_to_pb(v: RepoExecutionMetadata) -> pb::RepoExecutionMetadata {
    pb::RepoExecutionMetadata {
        base_ref: v.base_ref,
        target_branch: v.target_branch,
        branch: v.branch,
        head_sha: v.head_sha,
        is_dirty: v.is_dirty,
        changed_files: v.changed_files,
        untracked_files: v.untracked_files,
    }
}

fn repo_meta_from_pb(v: pb::RepoExecutionMetadata) -> RepoExecutionMetadata {
    RepoExecutionMetadata {
        base_ref: v.base_ref,
        target_branch: v.target_branch,
        branch: v.branch,
        head_sha: v.head_sha,
        is_dirty: v.is_dirty,
        changed_files: v.changed_files,
        untracked_files: v.untracked_files,
    }
}

fn github_pr_to_pb(v: GithubPullRequestMetadata) -> pb::GithubPullRequestMetadata {
    pb::GithubPullRequestMetadata {
        number: v.number,
        url: v.url,
        state: v.state,
        is_draft: v.is_draft,
        head_ref_name: v.head_ref_name,
        base_ref_name: v.base_ref_name,
    }
}

fn github_pr_from_pb(v: pb::GithubPullRequestMetadata) -> GithubPullRequestMetadata {
    GithubPullRequestMetadata {
        number: v.number,
        url: v.url,
        state: v.state,
        is_draft: v.is_draft,
        head_ref_name: v.head_ref_name,
        base_ref_name: v.base_ref_name,
    }
}

fn job_result_to_pb(v: JobResult) -> Result<pb::JobResult, Status> {
    Ok(pb::JobResult {
        outcome: execution_outcome_to_pb(v.outcome),
        termination_reason: termination_reason_to_pb(v.termination_reason),
        exit_code: v.exit_code,
        timing: Some(timing_to_pb(v.timing)),
        artifacts: Some(artifact_to_pb(v.artifacts)),
        output_json: v.output_json.map(json_to_pb),
        repo: v.repo.map(repo_meta_to_pb),
        github_pr: v.github_pr.map(github_pr_to_pb),
        error_message: v.error_message,
    })
}

fn job_result_from_pb(v: pb::JobResult) -> Result<JobResult, Status> {
    Ok(JobResult {
        outcome: execution_outcome_from_pb(v.outcome)?,
        termination_reason: termination_reason_from_pb(v.termination_reason)?,
        exit_code: v.exit_code,
        timing: timing_from_pb(
            v.timing
                .ok_or_else(|| Status::invalid_argument("missing timing"))?,
        )?,
        artifacts: artifact_from_pb(
            v.artifacts
                .ok_or_else(|| Status::invalid_argument("missing artifacts"))?,
        ),
        output_json: v.output_json.map(json_from_pb).transpose()?,
        repo: v.repo.map(repo_meta_from_pb),
        github_pr: v.github_pr.map(github_pr_from_pb),
        error_message: v.error_message,
    })
}

pub(crate) fn job_to_pb(v: Job) -> Result<pb::Job, Status> {
    let context_json = serde_json::to_vec(&v.context)
        .map_err(|error| Status::internal(format!("serialize job context: {error}")))?;
    let artifact = v.canonical_authorization.ok_or_else(|| {
        Status::failed_precondition(
            "job is incompatible: missing canonical authorization bytes; recreate it with a current server",
        )
    })?;
    let authorization_id = v
        .context
        .get(protocol::CONTEXT_AUTHORIZATION_ID_KEY)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Status::failed_precondition("job has no authorization identity"))?;
    let bundle = protocol::ImmutableAuthorizationBundle::verify(&artifact, authorization_id)
        .map_err(Status::failed_precondition)?;
    let canonical_authorization = bundle.canonical_bytes;
    let canonical_authorization_version = bundle.version;
    let canonical_authorization_digest = bundle.digest;
    Ok(pb::Job {
        id: v.id.to_string(),
        work_item_id: v.work_item_id.to_string(),
        step_id: v.step_id,
        state: job_state_to_pb(v.state),
        assigned_worker_id: v.assigned_worker_id,
        assigned_worker_session_id: v
            .assigned_worker_session_id
            .map(|value| value.to_string())
            .unwrap_or_default(),
        assigned_worker_session_generation: v
            .assigned_worker_session_generation
            .unwrap_or_default(),
        execution_mode: execution_mode_to_pb(v.execution_mode),
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_to_pb),
        context: Some(json_to_pb(v.context)),
        repo: v.repo.map(repo_to_pb),
        timeout_secs: v.timeout_secs,
        max_retries: v.max_retries,
        attempt: v.attempt,
        transcript_dir: v.transcript_dir,
        result: v.result.map(job_result_to_pb).transpose()?,
        lease_expires_at: v.lease_expires_at.map(ts_to_pb),
        created_at: Some(ts_to_pb(v.created_at)),
        updated_at: Some(ts_to_pb(v.updated_at)),
        context_json,
        canonical_authorization,
        canonical_authorization_version,
        canonical_authorization_digest,
    })
}

#[cfg(test)]
fn job_from_pb(v: pb::Job) -> Result<Job, Status> {
    if v.context_json.is_empty() {
        return Err(Status::invalid_argument("missing lossless context_json"));
    }
    let context = serde_json::from_slice(&v.context_json)
        .map_err(|error| Status::invalid_argument(format!("invalid context_json: {error}")))?;
    let canonical_authorization = if v.canonical_authorization.is_empty() {
        None
    } else {
        Some(protocol::CanonicalAuthorizationArtifact {
            version: v.canonical_authorization_version,
            digest: v.canonical_authorization_digest,
            canonical_bytes: v.canonical_authorization,
        })
    };
    Ok(Job {
        id: Uuid::parse_str(&v.id)
            .map_err(|e| Status::invalid_argument(format!("invalid job.id: {e}")))?,
        work_item_id: Uuid::parse_str(&v.work_item_id)
            .map_err(|e| Status::invalid_argument(format!("invalid job.work_item_id: {e}")))?,
        step_id: v.step_id,
        state: job_state_from_pb(v.state)?,
        assigned_worker_id: v.assigned_worker_id,
        assigned_worker_session_id: (!v.assigned_worker_session_id.is_empty())
            .then(|| Uuid::parse_str(&v.assigned_worker_session_id))
            .transpose()
            .map_err(|error| {
                Status::invalid_argument(format!("invalid assigned worker session ID: {error}"))
            })?,
        assigned_worker_session_generation: (v.assigned_worker_session_generation != 0)
            .then_some(v.assigned_worker_session_generation),
        execution_mode: execution_mode_from_pb(v.execution_mode)?,
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_from_pb),
        context,
        canonical_authorization,
        repo: v.repo.map(repo_from_pb),
        timeout_secs: v.timeout_secs,
        max_retries: v.max_retries,
        attempt: v.attempt,
        transcript_dir: v.transcript_dir,
        result: v.result.map(job_result_from_pb).transpose()?,
        lease_expires_at: v.lease_expires_at.map(ts_from_pb).transpose()?,
        created_at: ts_from_pb(
            v.created_at
                .ok_or_else(|| Status::invalid_argument("missing created_at"))?,
        )?,
        updated_at: ts_from_pb(
            v.updated_at
                .ok_or_else(|| Status::invalid_argument("missing updated_at"))?,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::config::{AuthMode, Token};
    use prost::Message;
    use tonic::service::Interceptor;

    fn config(auth_mode: AuthMode, tokens: Vec<Token>) -> Arc<ServerConfig> {
        Arc::new(ServerConfig {
            auth_mode,
            tokens,
            listen_addr: "127.0.0.1:0".to_string(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: crate::server::config::ApprovalFeatureMode::Disabled,
            base_url: None,
            default_approval_mode: crate::server::sessions::SessionApprovalMode::Remote,
            workflows_dir: "workflows".to_string(),
            config_root: "../agent-hub-configs".to_string(),
            task_dir: "tasks".to_string(),
            task_adapter: "file".to_string(),
            vikunja_base_url: None,
            vikunja_token: None,
            vikunja_bot_username: None,
            vikunja_bot_password: None,
            vikunja_project_id: None,
            vikunja_label_prefix: None,
            orchestration_db_path: ":memory:".to_string(),
            artifact_store_root: "local-agent-data".to_string(),
            grpc_listen_addr: None,
            github_webhook_secret: None,
            github_polling_enabled: false,
            github_poll_interval_secs: 30,
        })
    }

    #[test]
    fn job_roundtrip_proto_mapping() {
        let now = Utc::now();
        let mut job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Running,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec!["hi".into()],
            env: std::collections::HashMap::new(),
            prompt: Some(PromptRef {
                system: Some("sys".into()),
                user: "user".into(),
            }),
            context: serde_json::from_str(r#"{"a":9007199254740993,"fraction":1.25}"#)
                .expect("lossless context fixture"),
            repo: Some(RepoSource {
                repo_url: "https://example/repo.git".into(),
                base_ref: "origin/main".into(),
                branch_name: Some("agent-hub/test".into()),
            }),
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: Some("/tmp/t".into()),
            result: None,
            lease_expires_at: Some(now),
            created_at: now,
            updated_at: now,
            canonical_authorization: None,
            assigned_worker_session_id: None,
            assigned_worker_session_generation: None,
        };
        let environment = protocol::canonical_environment_for_execution(
            ExecutionMode::Docker,
            job.container_image.clone(),
            job.timeout_secs,
        );
        let request = protocol::ResolvedAuthorizationRequest {
            version: protocol::AUTHORIZATION_REQUEST_VERSION.to_string(),
            invocation_id: job.work_item_id,
            work_unit_id: job.work_item_id,
            root_work_item_id: job.work_item_id,
            workflow_id: "workflow".to_string(),
            workflow_snapshot: serde_json::json!({"schema":"fixture"}),
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
                primary_repo_branch: job.repo.as_ref().and_then(|repo| repo.branch_name.clone()),
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
                provider: Some("fixture".to_string()),
                task_id: Some("fixture".to_string()),
            },
            resource_constraints: protocol::ResolvedAuthorizationResources {
                timeout_secs: job.timeout_secs,
                max_retries: job.max_retries,
                attempt: job.attempt,
            },
        };
        let bundle = protocol::ImmutableAuthorizationBundle::create(&request)
            .expect("canonical authorization bundle");
        protocol::project_authorization_bundle_into_job_context(&mut job.context, &bundle)
            .expect("project authorization bundle");
        job.canonical_authorization = Some(bundle.artifact());

        let pb = job_to_pb(job.clone()).expect("to pb");
        let pb = pb::Job::decode(pb.encode_to_vec().as_slice()).expect("protobuf roundtrip");
        let decoded = job_from_pb(pb).expect("from pb");
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.step_id, job.step_id);
        assert_eq!(decoded.execution_mode, job.execution_mode);
        assert_eq!(decoded.context, job.context);
        assert_eq!(decoded.canonical_authorization, job.canonical_authorization);
        assert_eq!(
            decoded.repo.unwrap().branch_name,
            Some("agent-hub/test".into())
        );
        let mut id_tamper = job;
        id_tamper.context[protocol::CONTEXT_AUTHORIZATION_ID_KEY] =
            serde_json::json!("attacker-id");
        assert!(job_to_pb(id_tamper).is_err());
    }

    #[test]
    fn worker_protocol_capability_roundtrips_and_rejects_legacy_values() {
        let request = pb::RegisterWorkerRequest {
            worker_id: "worker".to_string(),
            hostname: "host".to_string(),
            supported_modes: vec![pb::ExecutionMode::Docker as i32],
            protocol_capabilities: vec![
                pb::WorkerProtocolCapability::CanonicalAuthorizationV1 as i32,
            ],
        };
        let decoded = pb::RegisterWorkerRequest::decode(request.encode_to_vec().as_slice())
            .expect("registration protobuf roundtrip");
        assert_eq!(
            worker_protocol_capability_from_pb(decoded.protocol_capabilities[0]).unwrap(),
            WorkerProtocolCapability::CanonicalAuthorizationV1
        );
        assert!(
            worker_protocol_capability_from_pb(pb::WorkerProtocolCapability::Unspecified as i32)
                .is_err()
        );
        assert!(worker_protocol_capability_from_pb(i32::MAX).is_err());

        let response = pb::RegisterWorkerResponse {
            session_id: Uuid::new_v4().to_string(),
            session_token: "opaque-test-token".to_string(),
            session_generation: 7,
        };
        let decoded = pb::RegisterWorkerResponse::decode(response.encode_to_vec().as_slice())
            .expect("registration response protobuf roundtrip");
        assert_eq!(decoded.session_generation, 7);
    }

    #[test]
    fn worker_session_metadata_requires_complete_credential() {
        let request = Request::new(());
        assert!(worker_session_from_metadata(&request, true).is_err());

        let mut partial = Request::new(());
        partial.metadata_mut().insert(
            WORKER_SESSION_ID_METADATA,
            Uuid::new_v4().to_string().parse().unwrap(),
        );
        assert!(worker_session_from_metadata(&partial, true).is_err());

        let session_id = Uuid::new_v4();
        let mut complete = Request::new(());
        complete.metadata_mut().insert(
            WORKER_SESSION_ID_METADATA,
            session_id.to_string().parse().unwrap(),
        );
        complete.metadata_mut().insert(
            WORKER_SESSION_TOKEN_METADATA,
            "opaque-test-token".parse().unwrap(),
        );
        let parsed = worker_session_from_metadata(&complete, true)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.session_token.expose(), "opaque-test-token");
    }

    #[test]
    fn grpc_auth_allows_when_auth_mode_none() {
        let mut interceptor = GrpcAuthInterceptor::new(config(AuthMode::None, vec![]));
        let req = Request::new(());
        assert!(interceptor.call(req).is_ok());
    }

    #[test]
    fn grpc_auth_rejects_missing_header_in_token_mode() {
        let mut interceptor = GrpcAuthInterceptor::new(config(
            AuthMode::Token,
            vec![Token {
                label: "desktop".into(),
                secret: protocol::Secret::new("abc123"),
            }],
        ));
        let req = Request::new(());
        assert!(interceptor.call(req).is_err());
    }

    #[test]
    fn grpc_auth_accepts_valid_bearer_header() {
        let mut interceptor = GrpcAuthInterceptor::new(config(
            AuthMode::Token,
            vec![Token {
                label: "desktop".into(),
                secret: protocol::Secret::new("abc123"),
            }],
        ));
        let mut req = Request::new(());
        req.metadata_mut().insert(
            "authorization",
            "Bearer abc123".parse().expect("valid metadata value"),
        );
        assert!(interceptor.call(req).is_ok());
    }

    #[test]
    fn github_pr_result_roundtrip_proto_mapping() {
        let now = Utc::now();
        let result = JobResult {
            outcome: ExecutionOutcome::Succeeded,
            termination_reason: TerminationReason::ExitCode,
            exit_code: Some(0),
            timing: ExecutionTiming {
                started_at: now,
                finished_at: now,
                duration_ms: 10,
            },
            artifacts: ArtifactSummary {
                stdout_path: "/tmp/stdout.log".into(),
                stderr_path: "/tmp/stderr.log".into(),
                output_path: Some("/tmp/output.json".into()),
                transcript_dir: Some("/tmp".into()),
                stdout_bytes: Some(1),
                stderr_bytes: Some(2),
                output_bytes: Some(3),
                structured_transcript_artifacts: vec![StructuredTranscriptArtifact {
                    key: "agent_context".into(),
                    path: "/tmp/.agent-context.json".into(),
                    bytes: Some(4),
                    schema: Some("agent_context_v1".into()),
                    record_count: None,
                    inline_json: Some(serde_json::json!({"schema": "agent_context_v1"})),
                }],
            },
            output_json: Some(serde_json::json!({"ok": true})),
            repo: None,
            github_pr: Some(GithubPullRequestMetadata {
                number: 42,
                url: "https://github.com/reddit/agent-hub/pull/42".into(),
                state: Some("open".into()),
                is_draft: Some(false),
                head_ref_name: Some("agent-hub/test".into()),
                base_ref_name: Some("main".into()),
            }),
            error_message: None,
        };

        let pb = job_result_to_pb(result).expect("to pb");
        let decoded = job_result_from_pb(pb).expect("from pb");
        assert_eq!(decoded.github_pr.as_ref().map(|p| p.number), Some(42));
        assert_eq!(decoded.artifacts.structured_transcript_artifacts.len(), 1);
        assert_eq!(
            decoded.artifacts.structured_transcript_artifacts[0].key,
            "agent_context"
        );
        assert_eq!(
            decoded.artifacts.structured_transcript_artifacts[0]
                .schema
                .as_deref(),
            Some("agent_context_v1")
        );
        assert_eq!(
            decoded.artifacts.structured_transcript_artifacts[0]
                .inline_json
                .as_ref()
                .and_then(|v| v.get("schema"))
                .and_then(serde_json::Value::as_str),
            Some("agent_context_v1")
        );
    }

    #[test]
    fn job_event_kind_roundtrip() {
        let pb = job_event_kind_to_pb(JobEventKind::Stdout);
        let decoded = job_event_kind_from_pb(pb).expect("decode");
        assert_eq!(decoded, JobEventKind::Stdout);
    }
}

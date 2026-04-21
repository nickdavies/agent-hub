#![allow(clippy::result_large_err)]

use chrono::{DateTime, Utc};
use protocol::{
    ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming, GithubPullRequestMetadata,
    HeartbeatRequest, Job, JobEvent, JobEventKind, JobResult, JobState, JobUpdateRequest,
    PollJobRequest, PromptRef, RegisterWorkerRequest, RepoExecutionMetadata, RepoSource,
    StructuredTranscriptArtifact, TerminationReason, WorkerState,
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
        let req = request.into_inner();
        let supported_modes = req
            .supported_modes
            .iter()
            .copied()
            .map(execution_mode_from_pb)
            .collect::<Result<Vec<_>, _>>()?;

        self.orchestration
            .register_worker(RegisterWorkerRequest {
                id: req.worker_id,
                hostname: req.hostname,
                supported_modes,
            })
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::RegisterWorkerResponse {}))
    }

    async fn heartbeat_worker(
        &self,
        request: Request<pb::HeartbeatWorkerRequest>,
    ) -> Result<Response<pb::HeartbeatWorkerResponse>, Status> {
        let req = request.into_inner();
        let state = worker_state_from_pb(req.state)?;
        self.orchestration
            .heartbeat(req.worker_id, HeartbeatRequest { state })
            .await
            .map_err(map_orchestration_error)?;

        Ok(Response::new(pb::HeartbeatWorkerResponse {}))
    }

    async fn poll_job(
        &self,
        request: Request<pb::PollJobRequest>,
    ) -> Result<Response<pb::PollJobResponse>, Status> {
        let req = request.into_inner();
        let response = self
            .orchestration
            .poll_job(PollJobRequest {
                worker_id: req.worker_id,
            })
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
        let req = request.into_inner();
        let state = job_state_from_pb(req.state)?;
        let job_id = Uuid::parse_str(&req.job_id)
            .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;
        let transcript_dir = req.transcript_dir;
        let result = req.result.map(job_result_from_pb).transpose()?;

        self.orchestration
            .update_job(
                job_id,
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
        let mut stream = request.into_inner();
        let mut accepted: u32 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Status::internal(e.to_string()))?;
            let job_id = Uuid::parse_str(&chunk.job_id)
                .map_err(|e| Status::invalid_argument(format!("invalid job id: {e}")))?;

            let kind = job_event_kind_from_pb(chunk.kind)?;
            let at = chunk
                .at
                .map(ts_from_pb)
                .transpose()?
                .unwrap_or_else(Utc::now);

            self.job_events
                .push(JobEvent {
                    job_id,
                    worker_id: chunk.worker_id,
                    at,
                    kind,
                    stream: chunk.stream,
                    message: chunk.message,
                })
                .await;
            accepted = accepted.saturating_add(1);
        }

        Ok(Response::new(pb::StreamJobEventsResponse { accepted }))
    }
}

fn execution_mode_to_pb(mode: ExecutionMode) -> i32 {
    match mode {
        ExecutionMode::Raw => pb::ExecutionMode::Raw as i32,
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
        pb::ExecutionMode::Raw => Ok(ExecutionMode::Raw),
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

fn job_to_pb(v: Job) -> Result<pb::Job, Status> {
    Ok(pb::Job {
        id: v.id.to_string(),
        work_item_id: v.work_item_id.to_string(),
        step_id: v.step_id,
        state: job_state_to_pb(v.state),
        assigned_worker_id: v.assigned_worker_id,
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
    })
}

#[cfg(test)]
fn job_from_pb(v: pb::Job) -> Result<Job, Status> {
    Ok(Job {
        id: Uuid::parse_str(&v.id)
            .map_err(|e| Status::invalid_argument(format!("invalid job.id: {e}")))?,
        work_item_id: Uuid::parse_str(&v.work_item_id)
            .map_err(|e| Status::invalid_argument(format!("invalid job.work_item_id: {e}")))?,
        step_id: v.step_id,
        state: job_state_from_pb(v.state)?,
        assigned_worker_id: v.assigned_worker_id,
        execution_mode: execution_mode_from_pb(v.execution_mode)?,
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_from_pb),
        context: json_from_pb(
            v.context
                .ok_or_else(|| Status::invalid_argument("missing context"))?,
        )?,
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
            github_token: None,
            github_api_base_url: "https://api.github.com".to_string(),
        })
    }

    #[test]
    fn job_roundtrip_proto_mapping() {
        let now = Utc::now();
        let job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Running,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec!["hi".into()],
            env: std::collections::HashMap::new(),
            prompt: Some(PromptRef {
                system: Some("sys".into()),
                user: "user".into(),
            }),
            context: serde_json::json!({"a":1}),
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
        };

        let pb = job_to_pb(job.clone()).expect("to pb");
        let decoded = job_from_pb(pb).expect("from pb");
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.step_id, job.step_id);
        assert_eq!(decoded.execution_mode, job.execution_mode);
        assert_eq!(
            decoded.repo.unwrap().branch_name,
            Some("agent-hub/test".into())
        );
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

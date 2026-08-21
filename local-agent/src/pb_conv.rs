use anyhow::Context;
use chrono::Utc;
use protocol::{
    ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming, GithubPullRequestMetadata,
    Job, JobResult, JobState, RepoExecutionMetadata, StructuredTranscriptArtifact,
    TerminationReason, WorkerState,
};

use crate::pb;

pub fn protocol_job_event_kind(kind: pb::JobEventKind) -> protocol::JobEventKind {
    match kind {
        pb::JobEventKind::JobEventStdout => protocol::JobEventKind::Stdout,
        pb::JobEventKind::JobEventStderr => protocol::JobEventKind::Stderr,
        pb::JobEventKind::JobEventSystem | pb::JobEventKind::JobEventUnspecified => {
            protocol::JobEventKind::System
        }
    }
}

pub fn execution_mode_to_pb(v: ExecutionMode) -> i32 {
    match v {
        ExecutionMode::Docker => pb::ExecutionMode::Docker as i32,
    }
}

pub fn worker_state_to_pb(v: WorkerState) -> i32 {
    match v {
        WorkerState::Idle => pb::WorkerState::Idle as i32,
        WorkerState::Busy => pb::WorkerState::Busy as i32,
        WorkerState::Offline => pb::WorkerState::Offline as i32,
    }
}

pub fn job_state_to_pb(v: JobState) -> i32 {
    match v {
        JobState::Pending => pb::JobState::Pending as i32,
        JobState::Assigned => pb::JobState::Assigned as i32,
        JobState::Running => pb::JobState::Running as i32,
        JobState::Succeeded => pb::JobState::Succeeded as i32,
        JobState::Failed => pb::JobState::Failed as i32,
        JobState::Cancelled => pb::JobState::Cancelled as i32,
    }
}

pub(crate) fn execution_mode_from_pb(v: i32) -> anyhow::Result<ExecutionMode> {
    Ok(
        match pb::ExecutionMode::try_from(v).unwrap_or(pb::ExecutionMode::Unspecified) {
            pb::ExecutionMode::Docker => ExecutionMode::Docker,
            pb::ExecutionMode::Unspecified => anyhow::bail!("execution mode unspecified"),
        },
    )
}

pub(crate) fn job_state_from_pb(v: i32) -> anyhow::Result<JobState> {
    Ok(
        match pb::JobState::try_from(v).unwrap_or(pb::JobState::JobUnspecified) {
            pb::JobState::Pending => JobState::Pending,
            pb::JobState::Assigned => JobState::Assigned,
            pb::JobState::Running => JobState::Running,
            pb::JobState::Succeeded => JobState::Succeeded,
            pb::JobState::Failed => JobState::Failed,
            pb::JobState::Cancelled => JobState::Cancelled,
            pb::JobState::JobUnspecified => anyhow::bail!("job state unspecified"),
        },
    )
}

fn execution_outcome_to_pb(v: ExecutionOutcome) -> i32 {
    match v {
        ExecutionOutcome::Succeeded => pb::ExecutionOutcome::OutcomeSucceeded as i32,
        ExecutionOutcome::Failed => pb::ExecutionOutcome::OutcomeFailed as i32,
        ExecutionOutcome::TimedOut => pb::ExecutionOutcome::OutcomeTimedOut as i32,
        ExecutionOutcome::Cancelled => pb::ExecutionOutcome::OutcomeCancelled as i32,
    }
}

fn execution_outcome_from_pb(v: i32) -> anyhow::Result<ExecutionOutcome> {
    Ok(
        match pb::ExecutionOutcome::try_from(v).unwrap_or(pb::ExecutionOutcome::OutcomeUnspecified)
        {
            pb::ExecutionOutcome::OutcomeSucceeded => ExecutionOutcome::Succeeded,
            pb::ExecutionOutcome::OutcomeFailed => ExecutionOutcome::Failed,
            pb::ExecutionOutcome::OutcomeTimedOut => ExecutionOutcome::TimedOut,
            pb::ExecutionOutcome::OutcomeCancelled => ExecutionOutcome::Cancelled,
            pb::ExecutionOutcome::OutcomeUnspecified => {
                anyhow::bail!("execution outcome unspecified")
            }
        },
    )
}

fn termination_reason_to_pb(v: TerminationReason) -> i32 {
    match v {
        TerminationReason::ExitCode => pb::TerminationReason::ReasonExitCode as i32,
        TerminationReason::Timeout => pb::TerminationReason::ReasonTimeout as i32,
        TerminationReason::Cancelled => pb::TerminationReason::ReasonCancelled as i32,
        TerminationReason::WorkerError => pb::TerminationReason::ReasonWorkerError as i32,
    }
}

fn termination_reason_from_pb(v: i32) -> anyhow::Result<TerminationReason> {
    Ok(
        match pb::TerminationReason::try_from(v).unwrap_or(pb::TerminationReason::ReasonUnspecified)
        {
            pb::TerminationReason::ReasonExitCode => TerminationReason::ExitCode,
            pb::TerminationReason::ReasonTimeout => TerminationReason::Timeout,
            pb::TerminationReason::ReasonCancelled => TerminationReason::Cancelled,
            pb::TerminationReason::ReasonWorkerError => TerminationReason::WorkerError,
            pb::TerminationReason::ReasonUnspecified => {
                anyhow::bail!("termination reason unspecified")
            }
        },
    )
}

pub fn ts_to_pb(v: chrono::DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: v.timestamp(),
        nanos: v.timestamp_subsec_nanos() as i32,
    }
}

fn ts_from_pb(v: prost_types::Timestamp) -> anyhow::Result<chrono::DateTime<Utc>> {
    chrono::DateTime::from_timestamp(v.seconds, v.nanos as u32)
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp"))
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

fn json_from_pb(v: prost_types::Value) -> anyhow::Result<serde_json::Value> {
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
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        Some(prost_types::value::Kind::StructValue(s)) => serde_json::Value::Object(
            s.fields
                .into_iter()
                .map(|(k, v)| Ok((k, json_from_pb(v)?)))
                .collect::<anyhow::Result<serde_json::Map<_, _>>>()?,
        ),
    })
}

fn prompt_to_pb(v: protocol::PromptRef) -> pb::PromptRef {
    pb::PromptRef {
        system: v.system,
        user: v.user,
    }
}

fn prompt_from_pb(v: pb::PromptRef) -> protocol::PromptRef {
    protocol::PromptRef {
        system: v.system,
        user: v.user,
    }
}

fn repo_to_pb(v: protocol::RepoSource) -> pb::RepoSource {
    pb::RepoSource {
        repo_url: v.repo_url,
        base_ref: v.base_ref,
        branch_name: v.branch_name,
    }
}

fn repo_from_pb(v: pb::RepoSource) -> protocol::RepoSource {
    protocol::RepoSource {
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

fn timing_from_pb(v: pb::ExecutionTiming) -> anyhow::Result<ExecutionTiming> {
    Ok(ExecutionTiming {
        started_at: ts_from_pb(v.started_at.context("missing started_at")?)?,
        finished_at: ts_from_pb(v.finished_at.context("missing finished_at")?)?,
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

pub fn job_result_to_pb(v: JobResult) -> pb::JobResult {
    pb::JobResult {
        outcome: execution_outcome_to_pb(v.outcome),
        termination_reason: termination_reason_to_pb(v.termination_reason),
        exit_code: v.exit_code,
        timing: Some(timing_to_pb(v.timing)),
        artifacts: Some(artifact_to_pb(v.artifacts)),
        output_json: v.output_json.map(json_to_pb),
        repo: v.repo.map(repo_meta_to_pb),
        github_pr: v.github_pr.map(github_pr_to_pb),
        error_message: v.error_message,
    }
}

pub(crate) fn job_result_from_pb(v: pb::JobResult) -> anyhow::Result<JobResult> {
    Ok(JobResult {
        outcome: execution_outcome_from_pb(v.outcome)?,
        termination_reason: termination_reason_from_pb(v.termination_reason)?,
        exit_code: v.exit_code,
        timing: timing_from_pb(v.timing.context("missing timing")?)?,
        artifacts: artifact_from_pb(v.artifacts.context("missing artifacts")?),
        output_json: v.output_json.map(json_from_pb).transpose()?,
        repo: v.repo.map(repo_meta_from_pb),
        github_pr: v.github_pr.map(github_pr_from_pb),
        error_message: v.error_message,
    })
}

pub fn job_from_pb(v: pb::Job) -> anyhow::Result<Job> {
    anyhow::ensure!(!v.context_json.is_empty(), "missing lossless context_json");
    let context: serde_json::Value =
        serde_json::from_slice(&v.context_json).context("invalid lossless context_json")?;
    anyhow::ensure!(
        !v.canonical_authorization.is_empty(),
        "job is incompatible: missing canonical authorization bytes"
    );
    let canonical_authorization = protocol::CanonicalAuthorizationArtifact {
        version: v.canonical_authorization_version,
        digest: v.canonical_authorization_digest,
        canonical_bytes: v.canonical_authorization,
    };
    Ok(Job {
        id: uuid::Uuid::parse_str(&v.id).context("invalid job id")?,
        work_item_id: uuid::Uuid::parse_str(&v.work_item_id).context("invalid work_item_id")?,
        step_id: v.step_id,
        state: job_state_from_pb(v.state)?,
        assigned_worker_id: v.assigned_worker_id,
        assigned_worker_session_id: (!v.assigned_worker_session_id.is_empty())
            .then(|| uuid::Uuid::parse_str(&v.assigned_worker_session_id))
            .transpose()
            .context("invalid assigned worker session ID")?,
        assigned_worker_session_generation: (v.assigned_worker_session_generation != 0)
            .then_some(v.assigned_worker_session_generation),
        execution_mode: execution_mode_from_pb(v.execution_mode)?,
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_from_pb),
        context,
        canonical_authorization: Some(canonical_authorization),
        repo: v.repo.map(repo_from_pb),
        timeout_secs: v.timeout_secs,
        max_retries: v.max_retries,
        attempt: v.attempt,
        transcript_dir: v.transcript_dir,
        result: v.result.map(job_result_from_pb).transpose()?,
        lease_expires_at: v.lease_expires_at.map(ts_from_pb).transpose()?,
        created_at: ts_from_pb(v.created_at.context("missing created_at")?)?,
        updated_at: ts_from_pb(v.updated_at.context("missing updated_at")?)?,
    })
}

pub fn job_to_pb(v: Job) -> pb::Job {
    let context_json = serde_json::to_vec(&v.context).expect("serialize job context");
    let (canonical_authorization, canonical_authorization_version, canonical_authorization_digest) =
        v.canonical_authorization
            .map(|artifact| (artifact.canonical_bytes, artifact.version, artifact.digest))
            .unwrap_or_default();
    pb::Job {
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
        result: v.result.map(job_result_to_pb),
        lease_expires_at: v.lease_expires_at.map(ts_to_pb),
        created_at: Some(ts_to_pb(v.created_at)),
        updated_at: Some(ts_to_pb(v.updated_at)),
        context_json,
        canonical_authorization,
        canonical_authorization_version,
        canonical_authorization_digest,
    }
}

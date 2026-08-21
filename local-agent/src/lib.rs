use protocol::{ImmutableAuthorizationBundle, Job};
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("worker");
}

pub mod pb_conv;

pub enum RunningTransitionOutcome {
    Applied,
    Cancelled,
    Rejected(String),
}

pub struct RunningExecutionPermit(());

pub fn running_execution_permit(
    outcome: RunningTransitionOutcome,
) -> Result<Option<RunningExecutionPermit>, String> {
    match outcome {
        RunningTransitionOutcome::Applied => Ok(Some(RunningExecutionPermit(()))),
        RunningTransitionOutcome::Cancelled => Ok(None),
        RunningTransitionOutcome::Rejected(error) => Err(error),
    }
}

pub fn verify_authorized_job(
    job: &Job,
    worker_id: &str,
    session_id: Uuid,
    generation: u64,
) -> Result<ImmutableAuthorizationBundle, String> {
    let authorization_id = job
        .context
        .get(protocol::CONTEXT_AUTHORIZATION_ID_KEY)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "job has no immutable authorization identity".to_string())?;
    let request_digest = job
        .context
        .get(protocol::CONTEXT_REQUEST_DIGEST_KEY)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "job has no immutable request digest".to_string())?;
    let artifact = job
        .canonical_authorization
        .as_ref()
        .ok_or_else(|| "job is incompatible: missing canonical authorization bytes".to_string())?;
    if artifact.digest != request_digest {
        return Err(format!(
            "job canonical authorization digest mismatch for version {}",
            artifact.version
        ));
    }
    let bundle = ImmutableAuthorizationBundle::verify(artifact, authorization_id)?;
    if bundle.request.work_unit_id != job.work_item_id
        || bundle.request.state_id != job.step_id
        || !bundle.request.matches_job_execution(job)
    {
        return Err("job execution fields do not match immutable authorization".to_string());
    }
    if job.assigned_worker_id.as_deref() != Some(worker_id)
        || job.assigned_worker_session_id != Some(session_id)
        || job.assigned_worker_session_generation != Some(generation)
    {
        return Err("job assignment does not match authenticated worker session".to_string());
    }
    Ok(bundle)
}

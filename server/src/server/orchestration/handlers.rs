use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use protocol::{
    CreateWorkItemRequest, FireEventRequest, HeartbeatRequest, Job, JobState, JobUpdateRequest,
    PollJobRequest, PollJobResponse, ReactiveWorkflowDefinition, RegisterWorkerRequest,
    RegisterWorkerResponse, WorkItem, WorkerInfo, WorkerSessionCredential,
};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::error::AppError;

use super::map_db;

const WORKER_SESSION_ID_HEADER: &str = "x-agent-hub-worker-session-id";
const WORKER_SESSION_TOKEN_HEADER: &str = "x-agent-hub-worker-session-token";

fn server_capability_headers(capabilities: &[protocol::WorkerProtocolCapability]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-agent-hub-server-capabilities",
        if capabilities.contains(&protocol::WorkerProtocolCapability::CanonicalAuthorizationV3) {
            "canonical_authorization_v1, canonical_authorization_v2, canonical_authorization_v3"
                .parse()
                .unwrap()
        } else if capabilities
            .contains(&protocol::WorkerProtocolCapability::CanonicalAuthorizationV2)
        {
            "canonical_authorization_v1, canonical_authorization_v2"
                .parse()
                .unwrap()
        } else {
            "canonical_authorization_v1".parse().unwrap()
        },
    );
    headers
}

fn worker_session_from_headers(
    headers: &HeaderMap,
    required: bool,
) -> Result<Option<WorkerSessionCredential>, AppError> {
    let session_id = headers.get(WORKER_SESSION_ID_HEADER);
    let session_token = headers.get(WORKER_SESSION_TOKEN_HEADER);
    match (session_id, session_token) {
        (None, None) if !required => Ok(None),
        (Some(session_id), Some(session_token)) => {
            let session_id = session_id
                .to_str()
                .ok()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    AppError::WorkerSessionUnauthorized(
                        "worker session ID is missing or invalid".to_string(),
                    )
                })?;
            let session_token = session_token.to_str().map_err(|_| {
                AppError::WorkerSessionUnauthorized(
                    "worker session token is missing or invalid".to_string(),
                )
            })?;
            if session_token.is_empty() {
                return Err(AppError::WorkerSessionUnauthorized(
                    "worker session token is missing or invalid".to_string(),
                ));
            }
            Ok(Some(WorkerSessionCredential {
                session_id,
                session_token: protocol::Secret::new(session_token),
            }))
        }
        _ => Err(AppError::WorkerSessionUnauthorized(
            "worker session credential is incomplete".to_string(),
        )),
    }
}

pub async fn create_work_item<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    Json(req): Json<CreateWorkItemRequest>,
) -> Result<Json<WorkItem>, AppError> {
    Ok(Json(state.orchestration.create_work_item(req).await?))
}

pub async fn list_work_items<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Result<Json<Vec<WorkItem>>, AppError> {
    Ok(Json(state.orchestration.list_work_items().await?))
}

pub async fn fire_work_item_event<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    Path(id): Path<Uuid>,
    Json(req): Json<FireEventRequest>,
) -> Result<Json<WorkItem>, AppError> {
    Ok(Json(state.orchestration.fire_event(id, req).await?))
}

pub async fn list_jobs<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Result<Json<Vec<Job>>, AppError> {
    Ok(Json(state.orchestration.list_jobs().await?))
}

pub async fn update_job<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<JobUpdateRequest>,
) -> Result<Json<Job>, AppError> {
    let credential = worker_session_from_headers(&headers, true)?.expect("required session");
    let job = state
        .orchestration
        .update_job_authenticated(id, credential, req)
        .await?;
    if matches!(
        job.state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    ) {
        state.job_events.mark_terminal(job.id).await;
        state.job_events.push(terminal_job_event(&job)).await;
    }
    Ok(Json(job))
}

pub async fn job_events_sse<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let job = state
        .orchestration
        .get_job(id)
        .await
        .map_err(map_db)?
        .ok_or_else(|| AppError::JobNotFound(id.to_string()))?;
    let (replay, rx) = state.job_events.subscribe(id).await;
    let replay_stream = tokio_stream::iter(
        replay
            .into_iter()
            .map(|ev| Ok::<Event, Infallible>(to_sse_event(&ev))),
    );
    let done_items = if is_terminal_job_state(&job.state) {
        vec![Ok::<Event, Infallible>(done_sse_event(&job))]
    } else {
        Vec::new()
    };
    let done_stream = tokio_stream::iter(done_items);

    let live_stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(ev) => Some(Ok::<Event, Infallible>(to_sse_event(&ev))),
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            Some(Ok::<Event, Infallible>(
                Event::default().event("events_lagged").data(n.to_string()),
            ))
        }
    });
    Ok(
        Sse::new(replay_stream.chain(done_stream).chain(live_stream))
            .keep_alive(KeepAlive::default()),
    )
}

pub async fn register_worker<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    headers: HeaderMap,
    Json(req): Json<RegisterWorkerRequest>,
) -> Result<(HeaderMap, Json<RegisterWorkerResponse>), AppError> {
    let replacement = worker_session_from_headers(&headers, false)?;
    let mut registered = state
        .orchestration
        .register_worker_session(req, replacement)
        .await?;
    let response_headers = server_capability_headers(&registered.server_protocol_capabilities);
    registered.server_protocol_capabilities.clear();
    Ok((response_headers, Json(registered)))
}

pub async fn heartbeat_worker<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> Result<(HeaderMap, Json<WorkerInfo>), AppError> {
    let credential = worker_session_from_headers(&headers, true)?.expect("required session");
    let capabilities = state
        .orchestration
        .authorization_protocol_capabilities()
        .await?;
    Ok((
        server_capability_headers(&capabilities),
        Json(
            state
                .orchestration
                .heartbeat_authenticated(id, credential, req)
                .await?,
        ),
    ))
}

pub async fn list_workers<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Result<Json<Vec<WorkerInfo>>, AppError> {
    Ok(Json(state.orchestration.list_workers().await?))
}

pub async fn poll_job<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    headers: HeaderMap,
    Json(req): Json<PollJobRequest>,
) -> Result<(HeaderMap, Json<PollJobResponse>), AppError> {
    let credential = worker_session_from_headers(&headers, true)?.expect("required session");
    let capabilities = state
        .orchestration
        .authorization_protocol_capabilities()
        .await?;
    Ok((
        server_capability_headers(&capabilities),
        Json(
            state
                .orchestration
                .poll_job_authenticated(credential, req)
                .await?,
        ),
    ))
}

pub async fn dispatch<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Result<Json<DispatchResponse>, AppError> {
    let created = state.orchestration.dispatch_actionable().await?;
    Ok(Json(DispatchResponse {
        created_jobs: created,
    }))
}

#[derive(Debug, serde::Serialize)]
pub struct DispatchResponse {
    pub created_jobs: usize,
}

pub async fn list_workflows<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Json<Vec<ReactiveWorkflowDefinition>> {
    Json(state.orchestration.list_workflows())
}

pub async fn validate_workflows<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
) -> Json<protocol::WorkflowValidationReport> {
    Json(state.orchestration.workflow_validation_report())
}

pub(crate) fn to_sse_event(ev: &protocol::JobEvent) -> Event {
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let event_name = if ev.stream.as_deref() == Some("terminal") {
        "done"
    } else {
        match ev.kind {
            protocol::JobEventKind::System => "system",
            protocol::JobEventKind::Stdout => "stdout",
            protocol::JobEventKind::Stderr => "stderr",
        }
    };
    Event::default()
        .event(event_name)
        .id(ev.at.timestamp_millis().to_string())
        .data(data)
}

fn done_sse_event(job: &Job) -> Event {
    Event::default().event("done").data(
        serde_json::json!({
            "job_id": job.id,
            "state": job.state,
        })
        .to_string(),
    )
}

fn is_terminal_job_state(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

pub fn terminal_job_event(job: &Job) -> protocol::JobEvent {
    protocol::JobEvent {
        job_id: job.id,
        worker_id: job.assigned_worker_id.clone().unwrap_or_default(),
        at: chrono::Utc::now(),
        kind: protocol::JobEventKind::System,
        stream: Some("terminal".to_string()),
        message: serde_json::json!({
            "state": job.state,
        })
        .to_string(),
    }
}

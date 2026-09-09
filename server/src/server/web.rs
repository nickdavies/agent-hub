use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path as FsPath;
use tower_sessions::Session;
use uuid::Uuid;

use protocol::WorkflowEventKind;

use super::AppState;
use super::DEFAULT_WEB_HOME;
use super::oauth;
use super::orchestration::{GithubPrLifecycleState, GithubPrState, MergeReadiness};
use super::task_adapter::{
    HumanActionKind, TaskHumanAction, task_from_work_item_with_valid_next_events,
};

mod web_artifacts;
mod web_structured;
use web_artifacts::{
    ArtifactQuery, RetrievableArtifactRow, artifact_spec_for_key, resolve_job_artifact_path,
    retrievable_artifact_rows_for_job, sanitize_content_disposition_filename,
};
use web_structured::{
    conventional_artifact_rows, job_detail_structured_value_preview_rows,
    structured_io_namespace_rows, structured_transcript_artifact_rows,
    work_item_structured_artifact_preview_rows,
    work_item_structured_artifact_projection_preview_rows, work_item_structured_value_preview_rows,
};

#[cfg(test)]
use web_structured::{structured_inline_preview, truncate_preview};

// --- Templates ---

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    providers: Vec<String>,
    has_basic_auth: bool,
}

#[derive(Template)]
#[template(path = "orchestration_dashboard.html")]
struct OrchestrationDashboardTemplate {
    email: String,
    queue_depth: Vec<StateCountRow>,
    worker_health: Vec<StateCountRow>,
    job_stats: JobStatsRow,
    workers: Vec<WorkerRow>,
    recent_jobs: Vec<JobListRow>,
    blocked_work_items: Vec<WorkItemRow>,
    triage_work_items: Vec<WorkItemRow>,
    has_auth: bool,
    readwrite: bool,
}

#[derive(Template)]
#[template(path = "orchestration_work_items.html")]
struct OrchestrationWorkItemsTemplate {
    email: String,
    work_items: Vec<WorkItemRow>,
    has_auth: bool,
    readwrite: bool,
}

#[derive(Template)]
#[template(path = "orchestration_jobs.html")]
struct OrchestrationJobsTemplate {
    email: String,
    jobs: Vec<JobListRow>,
    has_auth: bool,
}

#[derive(Template)]
#[template(path = "orchestration_transcripts.html")]
struct OrchestrationTranscriptsTemplate {
    email: String,
    jobs: Vec<TranscriptJobRow>,
    has_auth: bool,
}

#[derive(Template)]
#[template(path = "orchestration_workers.html")]
struct OrchestrationWorkersTemplate {
    email: String,
    workers: Vec<WorkerRow>,
    has_auth: bool,
}

#[derive(Template)]
#[template(path = "orchestration_work_item_detail.html")]
struct OrchestrationWorkItemDetailTemplate {
    email: String,
    row: WorkItemRow,
    github_pr_state: Option<GithubPrStateRow>,
    merge_readiness: Option<MergeReadinessRow>,
    jobs: Vec<JobRow>,
    structured_artifact_previews: Vec<StructuredArtifactPreviewRow>,
    structured_artifact_projection_previews: Vec<StructuredValuePreviewRow>,
    structured_value_previews: Vec<StructuredValuePreviewRow>,
    structured_io_namespaces: Vec<StructuredIoNamespaceRow>,
    workflow_options: Vec<WorkflowOption>,
    has_auth: bool,
    readwrite: bool,
}

#[derive(Template)]
#[template(path = "orchestration_job_detail.html")]
struct OrchestrationJobDetailTemplate {
    email: String,
    job: JobRow,
    structured_value_previews: Vec<StructuredValuePreviewRow>,
    conventional_artifacts: Vec<ConventionalArtifactRow>,
    structured_transcript_artifacts: Vec<StructuredTranscriptArtifactRow>,
    retrievable_artifacts: Vec<RetrievableArtifactRow>,
    has_auth: bool,
}

#[derive(Clone)]
struct WorkItemRow {
    id: Uuid,
    title: String,
    workflow_id: String,
    current_step: String,
    state: String,
    projected_state: String,
    triage_status: String,
    blocked_reason: String,
    last_failure_reason: String,
    required_action_description: String,
    human_gate_state: String,
    valid_next_events: Vec<String>,
    allowed_actions: Vec<String>,
    quick_actions: Vec<String>,
}

#[derive(Clone)]
struct JobRow {
    id: Uuid,
    work_item_id: Uuid,
    step_id: String,
    state: String,
    assigned_worker_id: String,
    updated_at: String,
}

#[derive(Clone)]
struct JobListRow {
    id: Uuid,
    work_item_id: Uuid,
    step_id: String,
    state: String,
    execution_mode: String,
    assigned_worker_id: String,
    updated_at: String,
}

#[derive(Clone)]
struct TranscriptJobRow {
    id: Uuid,
    work_item_id: Uuid,
    step_id: String,
    state: String,
    transcript_dir: String,
    artifact_summary: String,
    created_at: String,
}

#[derive(Clone)]
struct WorkerRow {
    id: String,
    hostname: String,
    state: String,
    supported_modes: String,
    last_heartbeat_at: String,
}

#[derive(Clone)]
struct StructuredTranscriptArtifactRow {
    key: String,
    path: String,
    bytes: String,
    schema: String,
    record_count: String,
    context_alias: String,
    metadata_context_alias: String,
    data_context_alias: String,
    has_inline_preview: bool,
    inline_preview: String,
}

#[derive(Clone)]
struct ConventionalArtifactRow {
    key: String,
    path: String,
    bytes: String,
    context_alias: String,
    metadata_context_alias: String,
}

#[derive(Clone)]
struct WorkflowOption {
    id: String,
    kind: String,
}

#[derive(Clone)]
struct GithubPrStateRow {
    url: String,
    number: u64,
    state: String,
    approved: bool,
    checks_passing: bool,
    changes_requested: bool,
    mergeable: bool,
    last_updated: String,
}

#[derive(Clone)]
struct MergeReadinessRow {
    ready: bool,
    is_open: bool,
    approved: bool,
    checks_passing: bool,
    no_changes_requested: bool,
}

#[derive(Clone)]
struct StructuredIoNamespaceRow {
    name: String,
    aliases: Vec<StructuredIoAliasRow>,
    truncated: bool,
}

#[derive(Clone)]
struct StructuredIoAliasRow {
    alias: String,
    interpolation: String,
    value_kind: String,
}

#[derive(Clone)]
struct StructuredArtifactPreviewRow {
    step_id: String,
    artifact_key: String,
    data_context_alias: String,
    metadata_context_alias: String,
    schema: String,
    inline_preview: String,
}

#[derive(Clone)]
struct StructuredValuePreviewRow {
    namespace: String,
    step_id: String,
    context_alias: String,
    interpolation: String,
    value_kind: String,
    inline_preview: String,
}

#[derive(Clone)]
struct StateCountRow {
    label: String,
    count: usize,
}

#[derive(Clone)]
struct JobStatsRow {
    queued: usize,
    running: usize,
    completed: usize,
    failed: usize,
    cancelled: usize,
}

#[cfg(test)]
const STRUCTURED_IO_CONTEXT_NAMESPACES: [&str; 6] =
    web_structured::STRUCTURED_IO_CONTEXT_NAMESPACES;
#[cfg(test)]
const STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES: usize =
    web_structured::STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES;

// --- Handlers ---

/// GET /auth/login
pub async fn login_page(State(state): State<AppState>) -> Response {
    let (providers, has_basic_auth) = match &*state.oauth {
        Some(mgr) => (
            mgr.provider_names().into_iter().map(String::from).collect(),
            mgr.has_basic_auth(),
        ),
        None => (vec![], false),
    };
    into_html_response(LoginTemplate {
        providers,
        has_basic_auth,
    })
}

#[derive(Deserialize)]
pub struct BasicAuthForm {
    pub username: String,
    pub password: String,
}

/// POST /auth/login/basic — basic auth form submission.
pub async fn basic_auth_login(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<BasicAuthForm>,
) -> Response {
    let oauth = match &*state.oauth {
        Some(mgr) => mgr,
        None => return (StatusCode::NOT_FOUND, "Auth not configured").into_response(),
    };

    if !oauth.check_basic_auth(&form.username, &form.password) {
        return Redirect::to("/auth/login?error=invalid").into_response();
    }

    // Use the username as the "email" for session identity
    if let Err(e) = oauth::set_session_email(&session, &form.username).await {
        tracing::error!("failed to store session: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session error").into_response();
    }

    Redirect::to(DEFAULT_WEB_HOME).into_response()
}

/// GET /orchestration — summary dashboard
pub async fn orchestration_dashboard(State(state): State<AppState>, session: Session) -> Response {
    let email = session_email(&session).await;
    let readwrite = true;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let work_item_state_counts = match state.orchestration.work_item_state_counts().await {
        Ok(counts) => counts,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let worker_state_counts = match state.orchestration.worker_state_counts().await {
        Ok(counts) => counts,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let job_state_counts = match state.orchestration.job_state_counts().await {
        Ok(counts) => counts,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let workers = match state.orchestration.list_workers().await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let recent_jobs = match state.orchestration.list_recent_terminal_jobs(15).await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let blocked_work_items = match state.orchestration.list_human_gate_work_items(20).await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let triage_work_items = match state.orchestration.list_triage_work_items(20).await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let queue_depth = vec![
        StateCountRow {
            label: "Pending".to_string(),
            count: map_count(&work_item_state_counts, "pending"),
        },
        StateCountRow {
            label: "Active".to_string(),
            count: map_count(&work_item_state_counts, "running"),
        },
        StateCountRow {
            label: "Completed".to_string(),
            count: map_count(&work_item_state_counts, "succeeded"),
        },
        StateCountRow {
            label: "Failed".to_string(),
            count: map_count(&work_item_state_counts, "failed"),
        },
        StateCountRow {
            label: "Cancelled".to_string(),
            count: map_count(&work_item_state_counts, "cancelled"),
        },
    ];

    let worker_health = vec![
        StateCountRow {
            label: "Idle".to_string(),
            count: map_count(&worker_state_counts, "idle"),
        },
        StateCountRow {
            label: "Busy".to_string(),
            count: map_count(&worker_state_counts, "busy"),
        },
        StateCountRow {
            label: "Offline".to_string(),
            count: map_count(&worker_state_counts, "offline"),
        },
    ];

    let job_stats = JobStatsRow {
        queued: map_count(&job_state_counts, "pending") + map_count(&job_state_counts, "assigned"),
        running: map_count(&job_state_counts, "running"),
        completed: map_count(&job_state_counts, "succeeded"),
        failed: map_count(&job_state_counts, "failed"),
        cancelled: map_count(&job_state_counts, "cancelled"),
    };

    let workers = workers
        .into_iter()
        .map(|worker| WorkerRow {
            id: worker.id,
            hostname: worker.hostname,
            state: worker.state.to_string(),
            supported_modes: if worker.supported_modes.is_empty() {
                "none".to_string()
            } else {
                worker
                    .supported_modes
                    .into_iter()
                    .map(|mode| mode.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            last_heartbeat_at: worker
                .last_heartbeat_at
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string(),
        })
        .collect::<Vec<_>>();

    let recent_jobs = recent_jobs
        .into_iter()
        .map(|job| JobListRow {
            id: job.id,
            work_item_id: job.work_item_id,
            step_id: job.step_id,
            state: job.state.to_string(),
            execution_mode: job.execution_mode.to_string(),
            assigned_worker_id: job.assigned_worker_id.unwrap_or_default(),
            updated_at: job.updated_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        })
        .collect::<Vec<_>>();

    let blocked_work_items = blocked_work_items
        .iter()
        .map(|item| work_item_row_from_item(&state, item))
        .collect::<Vec<_>>();
    let triage_work_items = triage_work_items
        .iter()
        .map(|item| work_item_row_from_item(&state, item))
        .collect::<Vec<_>>();

    into_html_response(OrchestrationDashboardTemplate {
        email,
        queue_depth,
        worker_health,
        job_stats,
        workers,
        recent_jobs,
        blocked_work_items,
        triage_work_items,
        has_auth,
        readwrite,
    })
}

/// GET /orchestration/work-items — work-item operator view
pub async fn orchestration_work_items(State(state): State<AppState>, session: Session) -> Response {
    let email = session_email(&session).await;
    let readwrite = true;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let work_items = match state.orchestration.list_work_items().await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let rows = work_items
        .iter()
        .map(|item| work_item_row_from_item(&state, item))
        .collect::<Vec<_>>();

    into_html_response(OrchestrationWorkItemsTemplate {
        email,
        work_items: rows,
        has_auth,
        readwrite,
    })
}

/// GET /orchestration/jobs
pub async fn orchestration_jobs(State(state): State<AppState>, session: Session) -> Response {
    let email = session_email(&session).await;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let jobs = match state.orchestration.list_jobs().await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let rows = jobs
        .into_iter()
        .map(|job| JobListRow {
            id: job.id,
            work_item_id: job.work_item_id,
            step_id: job.step_id,
            state: job.state.to_string(),
            execution_mode: job.execution_mode.to_string(),
            assigned_worker_id: job.assigned_worker_id.unwrap_or_default(),
            updated_at: job.updated_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        })
        .collect::<Vec<_>>();

    into_html_response(OrchestrationJobsTemplate {
        email,
        jobs: rows,
        has_auth,
    })
}

/// GET /orchestration/transcripts
pub async fn orchestration_transcripts(
    State(state): State<AppState>,
    session: Session,
) -> Response {
    let email = session_email(&session).await;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let jobs = match state.orchestration.list_jobs_with_transcripts().await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let rows = jobs
        .into_iter()
        .map(|job| {
            let artifact_summary = job
                .result
                .as_ref()
                .map(|result| {
                    let count = result.artifacts.structured_transcript_artifacts.len();
                    let total_bytes = result
                        .artifacts
                        .structured_transcript_artifacts
                        .iter()
                        .filter_map(|artifact| artifact.bytes)
                        .sum::<u64>();
                    if total_bytes > 0 {
                        format!("{count} artifacts, {total_bytes} bytes")
                    } else {
                        format!("{count} artifacts")
                    }
                })
                .unwrap_or_else(|| "metadata unavailable".to_string());

            TranscriptJobRow {
                id: job.id,
                work_item_id: job.work_item_id,
                step_id: job.step_id,
                state: job.state.to_string(),
                transcript_dir: job.transcript_dir.unwrap_or_default(),
                artifact_summary,
                created_at: job.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            }
        })
        .collect::<Vec<_>>();

    into_html_response(OrchestrationTranscriptsTemplate {
        email,
        jobs: rows,
        has_auth,
    })
}

/// GET /orchestration/workers
pub async fn orchestration_workers(State(state): State<AppState>, session: Session) -> Response {
    let email = session_email(&session).await;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let workers = match state.orchestration.list_workers().await {
        Ok(items) => items,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let rows = workers
        .into_iter()
        .map(|worker| WorkerRow {
            id: worker.id,
            hostname: worker.hostname,
            state: worker.state.to_string(),
            supported_modes: if worker.supported_modes.is_empty() {
                "none".to_string()
            } else {
                worker
                    .supported_modes
                    .into_iter()
                    .map(|mode| mode.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            last_heartbeat_at: worker
                .last_heartbeat_at
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string(),
        })
        .collect::<Vec<_>>();

    into_html_response(OrchestrationWorkersTemplate {
        email,
        workers: rows,
        has_auth,
    })
}

/// GET /orchestration/work-items/{id}
pub async fn orchestration_work_item_detail(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<Uuid>,
) -> Response {
    let email = session_email(&session).await;
    let readwrite = true;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;

    let item = match state.orchestration.get_work_item(id).await {
        Ok(Some(item)) => item,
        Ok(None) => return (StatusCode::NOT_FOUND, "Work item not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let jobs = match state.orchestration.list_jobs_for_work_item(id).await {
        Ok(jobs) => jobs,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let jobs = jobs
        .into_iter()
        .map(|job| JobRow {
            id: job.id,
            work_item_id: job.work_item_id,
            step_id: job.step_id,
            state: job.state.to_string(),
            assigned_worker_id: job.assigned_worker_id.unwrap_or_default(),
            updated_at: job.updated_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        })
        .collect::<Vec<_>>();

    let valid_next_events = state.orchestration.valid_next_events_for_work_item(&item);
    let task = task_from_work_item_with_valid_next_events(&item, None, &valid_next_events);
    let allowed_actions = task
        .allowed_actions
        .iter()
        .map(human_action_name)
        .collect::<Vec<_>>();
    let quick_actions = allowed_actions
        .iter()
        .filter(|action| action.as_str() == "approve" || action.as_str() == "unblock")
        .cloned()
        .collect::<Vec<_>>();
    let github_pr_state = state
        .orchestration
        .github_pr_state_for_work_item(&item)
        .as_ref()
        .map(github_pr_state_row_from_state);
    let merge_readiness = state
        .orchestration
        .check_merge_readiness(&item)
        .as_ref()
        .map(merge_readiness_row_from_state);
    let row = WorkItemRow {
        id: item.id,
        title: item
            .title
            .clone()
            .unwrap_or_else(|| format!("Work item {}", item.id)),
        workflow_id: item.workflow_id,
        current_step: item.current_step,
        state: item.state.to_string(),
        projected_state: item
            .projected_state
            .unwrap_or_else(|| "unknown".to_string()),
        triage_status: item
            .triage_status
            .unwrap_or_else(|| format_task_triage_status(&task.triage_status)),
        blocked_reason: task.blocked_reason.unwrap_or_default(),
        last_failure_reason: task.last_failure_reason.unwrap_or_default(),
        required_action_description: task.required_action_description.unwrap_or_default(),
        human_gate_state: task
            .human_gate_state
            .as_ref()
            .map(format_task_human_gate_state)
            .unwrap_or_default(),
        valid_next_events: task
            .valid_next_events
            .iter()
            .map(workflow_event_name)
            .collect::<Vec<_>>(),
        allowed_actions,
        quick_actions,
    };

    let workflow_options = state
        .orchestration
        .list_workflows()
        .into_iter()
        .map(|workflow| WorkflowOption {
            id: workflow.id,
            kind: "reactive_v2".to_string(),
        })
        .collect::<Vec<_>>();

    let structured_io_namespaces = structured_io_namespace_rows(&item.context);
    let structured_artifact_previews = work_item_structured_artifact_preview_rows(&item.context);
    let structured_artifact_projection_previews =
        work_item_structured_artifact_projection_preview_rows(&item.context);
    let structured_value_previews = work_item_structured_value_preview_rows(&item.context);

    into_html_response(OrchestrationWorkItemDetailTemplate {
        email,
        row,
        github_pr_state,
        merge_readiness,
        jobs,
        structured_artifact_previews,
        structured_artifact_projection_previews,
        structured_value_previews,
        structured_io_namespaces,
        workflow_options,
        has_auth,
        readwrite,
    })
}

/// GET /orchestration/jobs/{id}
pub async fn orchestration_job_detail(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<Uuid>,
) -> Response {
    let email = session_email(&session).await;
    let has_auth = state.config.auth_mode != crate::server::config::AuthMode::None;
    let job = match state.orchestration.get_job(id).await {
        Ok(Some(job)) => job,
        Ok(None) => return (StatusCode::NOT_FOUND, "Job not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let step_id = job.step_id.clone();
    let row = JobRow {
        id: job.id,
        work_item_id: job.work_item_id,
        step_id,
        state: job.state.to_string(),
        assigned_worker_id: job.assigned_worker_id.unwrap_or_default(),
        updated_at: job.updated_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    };
    let structured_transcript_artifacts = job
        .result
        .as_ref()
        .map(|result| {
            structured_transcript_artifact_rows(
                &row.step_id,
                &result.artifacts.structured_transcript_artifacts,
            )
        })
        .unwrap_or_default();
    let conventional_artifacts = job
        .result
        .as_ref()
        .map(|result| conventional_artifact_rows(&row.step_id, &result.artifacts))
        .unwrap_or_default();
    let structured_value_previews =
        job_detail_structured_value_preview_rows(&row.step_id, job.result.as_ref());
    let retrievable_artifacts = job
        .result
        .as_ref()
        .map(|result| retrievable_artifact_rows_for_job(row.id, result))
        .unwrap_or_default();

    into_html_response(OrchestrationJobDetailTemplate {
        email,
        job: row,
        structured_value_previews,
        conventional_artifacts,
        structured_transcript_artifacts,
        retrievable_artifacts,
        has_auth,
    })
}

/// GET /orchestration/jobs/{id}/artifacts/{key}
pub async fn orchestration_job_artifact(
    State(state): State<AppState>,
    Path((id, key)): Path<(Uuid, String)>,
    Query(query): Query<ArtifactQuery>,
) -> Response {
    let Some(spec) = artifact_spec_for_key(&key) else {
        return (
            StatusCode::BAD_REQUEST,
            format!("unsupported artifact key: {key}"),
        )
            .into_response();
    };

    let job = match state.orchestration.get_job(id).await {
        Ok(Some(job)) => job,
        Ok(None) => return (StatusCode::NOT_FOUND, "Job not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(result) = job.result.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "Artifact '{}' not available: job has no persisted result",
                spec.key
            ),
        )
            .into_response();
    };

    let stored_key = match resolve_job_artifact_path(result, spec.key) {
        Ok(path) => path,
        Err(message) => return (StatusCode::NOT_FOUND, message).into_response(),
    };

    let body = match state.artifact_store.retrieve(&stored_key).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Artifact '{}' file is missing or stale", spec.key),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(
                job_id = %id,
                artifact_key = %spec.key,
                artifact_store_key = %stored_key,
                error = %e,
                "failed reading job artifact"
            );
            return (
                StatusCode::NOT_FOUND,
                format!("Artifact '{}' file is unreadable or stale", spec.key),
            )
                .into_response();
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(spec.content_type));

    if query.download == Some(1)
        && let Some(filename) = FsPath::new(&stored_key)
            .file_name()
            .and_then(|name| name.to_str())
            .map(sanitize_content_disposition_filename)
        && let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        headers.insert(CONTENT_DISPOSITION, value);
    }

    (StatusCode::OK, headers, body).into_response()
}

#[derive(Deserialize)]
pub struct OrchestrationActionForm {
    pub action: String,
    pub reason: Option<String>,
    pub workflow_id: Option<String>,
}

/// POST /orchestration/work-items/{id}/action
pub async fn orchestration_fire_work_item_action(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Form(form): Form<OrchestrationActionForm>,
) -> Response {
    let action = match parse_human_action(&form.action) {
        Some(action) => action,
        None => return (StatusCode::BAD_REQUEST, "Unknown action").into_response(),
    };

    let result = if action == HumanActionKind::ChooseWorkflow {
        let Some(workflow_id) = form.workflow_id.clone().filter(|v| !v.is_empty()) else {
            return (StatusCode::BAD_REQUEST, "workflow_id is required").into_response();
        };
        state.orchestration.choose_workflow(id, workflow_id).await
    } else {
        let action = TaskHumanAction {
            kind: action,
            workflow_id: None,
            reason: form.reason.clone().filter(|v| !v.is_empty()),
        };
        state.orchestration.apply_operator_action(id, action).await
    };

    match result {
        Ok(_) => Redirect::to(&format!("/orchestration/work-items/{id}")).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// Extract email from session. Middleware guarantees this exists on authed routes.
async fn session_email(session: &Session) -> String {
    oauth::get_session_email(session).await.unwrap_or_default()
}

fn into_html_response<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("template render error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

fn format_task_human_gate_state(state: &super::task_adapter::TaskHumanGateState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_task_triage_status(status: &super::task_adapter::TaskTriageStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_human_action(raw: &str) -> Option<HumanActionKind> {
    match raw {
        "choose_workflow" => Some(HumanActionKind::ChooseWorkflow),
        "submit" => Some(HumanActionKind::Submit),
        "approve" => Some(HumanActionKind::Approve),
        "request_changes" => Some(HumanActionKind::RequestChanges),
        "unblock" => Some(HumanActionKind::Unblock),
        "retry" => Some(HumanActionKind::Retry),
        "cancel" => Some(HumanActionKind::Cancel),
        "checks_passed" => Some(HumanActionKind::ChecksPassed),
        "checks_failed" => Some(HumanActionKind::ChecksFailed),
        _ => None,
    }
}

fn human_action_name(action: &HumanActionKind) -> String {
    match action {
        HumanActionKind::ChooseWorkflow => "choose_workflow".to_string(),
        HumanActionKind::Submit => "submit".to_string(),
        HumanActionKind::Approve => "approve".to_string(),
        HumanActionKind::RequestChanges => "request_changes".to_string(),
        HumanActionKind::Unblock => "unblock".to_string(),
        HumanActionKind::Retry => "retry".to_string(),
        HumanActionKind::Cancel => "cancel".to_string(),
        HumanActionKind::ChecksPassed => "checks_passed".to_string(),
        HumanActionKind::ChecksFailed => "checks_failed".to_string(),
    }
}

fn workflow_event_name(event: &WorkflowEventKind) -> String {
    event.to_string()
}

fn github_pr_state_name(state: &GithubPrLifecycleState) -> String {
    match state {
        GithubPrLifecycleState::Open => "open".to_string(),
        GithubPrLifecycleState::Closed => "closed".to_string(),
        GithubPrLifecycleState::Merged => "merged".to_string(),
    }
}

fn github_pr_state_row_from_state(state: &GithubPrState) -> GithubPrStateRow {
    GithubPrStateRow {
        url: state.url.clone(),
        number: state.number,
        state: github_pr_state_name(&state.state),
        approved: state.approved,
        checks_passing: state.checks_passing,
        changes_requested: state.changes_requested,
        mergeable: state.mergeable,
        last_updated: state
            .last_updated
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string(),
    }
}

fn merge_readiness_row_from_state(readiness: &MergeReadiness) -> MergeReadinessRow {
    MergeReadinessRow {
        ready: readiness.ready,
        is_open: readiness.is_open,
        approved: readiness.approved,
        checks_passing: readiness.checks_passing,
        no_changes_requested: readiness.no_changes_requested,
    }
}

fn map_count(map: &HashMap<String, usize>, key: &str) -> usize {
    map.get(key).copied().unwrap_or_default()
}

fn work_item_row_from_item(state: &AppState, item: &protocol::WorkItem) -> WorkItemRow {
    let valid_next_events = state.orchestration.valid_next_events_for_work_item(item);
    let task = task_from_work_item_with_valid_next_events(item, None, &valid_next_events);
    let title = item
        .title
        .clone()
        .unwrap_or_else(|| format!("Work item {}", item.id));
    let blocked_reason = task.blocked_reason.unwrap_or_default();
    let last_failure_reason = task.last_failure_reason.unwrap_or_default();
    let required_action_description = task.required_action_description.unwrap_or_default();
    let triage_status = item
        .triage_status
        .clone()
        .unwrap_or_else(|| format_task_triage_status(&task.triage_status));
    let allowed_actions = task
        .allowed_actions
        .iter()
        .map(human_action_name)
        .collect::<Vec<_>>();
    let quick_actions = allowed_actions
        .iter()
        .filter(|action| action.as_str() == "approve" || action.as_str() == "unblock")
        .cloned()
        .collect::<Vec<_>>();
    WorkItemRow {
        id: item.id,
        title,
        workflow_id: item.workflow_id.clone(),
        current_step: item.current_step.clone(),
        state: item.state.to_string(),
        projected_state: item
            .projected_state
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        triage_status,
        blocked_reason,
        last_failure_reason,
        required_action_description,
        human_gate_state: task
            .human_gate_state
            .as_ref()
            .map(format_task_human_gate_state)
            .unwrap_or_default(),
        valid_next_events: task
            .valid_next_events
            .iter()
            .map(workflow_event_name)
            .collect::<Vec<_>>(),
        allowed_actions,
        quick_actions,
    }
}

#[cfg(test)]
mod tests {
    use askama::Template as _;

    use super::web_artifacts::{
        resolve_job_artifact_path, resolve_job_artifact_path_in_store,
        retrievable_artifact_rows_for_job, sanitize_content_disposition_filename,
    };
    use super::web_structured::{
        TRANSCRIPT_JSONL_PREVIEW_MAX_LINE_CHARS, TRANSCRIPT_JSONL_PREVIEW_MAX_RECORDS,
    };
    use super::{
        JobRow, OrchestrationJobDetailTemplate, STRUCTURED_IO_CONTEXT_NAMESPACES,
        STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES, conventional_artifact_rows,
        job_detail_structured_value_preview_rows, structured_inline_preview,
        structured_io_namespace_rows, structured_transcript_artifact_rows, truncate_preview,
        work_item_structured_artifact_preview_rows,
        work_item_structured_artifact_projection_preview_rows,
        work_item_structured_value_preview_rows,
    };

    #[test]
    fn structured_io_namespace_rows_exposes_aliases_for_known_namespaces() {
        let context = serde_json::json!({
            "job_outputs": {
                "build": {
                    "message": "ok"
                }
            },
            "job_results": {
                "build": {
                    "outcome": "succeeded"
                }
            },
            "job_artifact_paths": {
                "build": {
                    "agent_context": "/tmp/job/.agent-context.json"
                }
            },
            "job_artifact_metadata": {
                "build": {
                    "agent_context": {
                        "path": "/tmp/job/.agent-context.json",
                        "schema": "agent_context_v1"
                    }
                }
            },
            "job_artifact_data": {
                "build": {
                    "agent_context": {
                        "schema": "agent_context_v1"
                    }
                }
            },
            "child_results": {
                "spawn": {
                    "job_results": {
                        "run": {
                            "outcome": "succeeded"
                        }
                    }
                }
            },
            "unrelated": {
                "should_not_show": true
            }
        });

        let rows = structured_io_namespace_rows(&context);
        assert_eq!(rows.len(), STRUCTURED_IO_CONTEXT_NAMESPACES.len());

        let output_aliases = rows
            .iter()
            .find(|row| row.name == "job_outputs")
            .expect("job_outputs row")
            .aliases
            .iter()
            .map(|row| row.alias.clone())
            .collect::<Vec<_>>();
        assert!(output_aliases.contains(&"job_outputs.build.message".to_string()));
        assert!(!output_aliases.contains(&"job_outputs".to_string()));

        let output_interpolation = rows
            .iter()
            .find(|row| row.name == "job_outputs")
            .expect("job_outputs row")
            .aliases
            .iter()
            .find(|row| row.alias == "job_outputs.build.message")
            .expect("leaf alias")
            .interpolation
            .clone();
        assert_eq!(output_interpolation, "{{job_outputs.build.message}}");

        let child_aliases = rows
            .iter()
            .find(|row| row.name == "child_results")
            .expect("child_results row")
            .aliases
            .iter()
            .map(|row| row.alias.clone())
            .collect::<Vec<_>>();
        assert!(child_aliases.contains(&"child_results.spawn.job_results.run.outcome".to_string()));

        let metadata_aliases = rows
            .iter()
            .find(|row| row.name == "job_artifact_metadata")
            .expect("job_artifact_metadata row")
            .aliases
            .iter()
            .map(|row| row.alias.clone())
            .collect::<Vec<_>>();
        assert!(
            metadata_aliases
                .contains(&"job_artifact_metadata.build.agent_context.schema".to_string())
        );

        let data_aliases = rows
            .iter()
            .find(|row| row.name == "job_artifact_data")
            .expect("job_artifact_data row")
            .aliases
            .iter()
            .map(|row| row.alias.clone())
            .collect::<Vec<_>>();
        assert!(data_aliases.contains(&"job_artifact_data.build.agent_context.schema".to_string()));
    }

    #[test]
    fn structured_io_namespace_rows_skip_container_only_aliases() {
        let context = serde_json::json!({
            "job_outputs": {
                "build": {
                    "nested": {
                        "message": "ok"
                    }
                }
            }
        });

        let rows = structured_io_namespace_rows(&context);
        let output_aliases = rows
            .iter()
            .find(|row| row.name == "job_outputs")
            .expect("job_outputs row")
            .aliases
            .iter()
            .map(|row| row.alias.clone())
            .collect::<Vec<_>>();

        assert!(!output_aliases.contains(&"job_outputs".to_string()));
        assert!(!output_aliases.contains(&"job_outputs.build".to_string()));
        assert!(!output_aliases.contains(&"job_outputs.build.nested".to_string()));
        assert!(output_aliases.contains(&"job_outputs.build.nested.message".to_string()));
    }

    #[test]
    fn structured_io_namespace_rows_return_empty_when_context_not_object() {
        let rows = structured_io_namespace_rows(&serde_json::Value::Null);
        assert_eq!(rows.len(), STRUCTURED_IO_CONTEXT_NAMESPACES.len());
        assert!(rows.iter().all(|row| row.aliases.is_empty()));
        assert!(rows.iter().all(|row| !row.truncated));
    }

    #[test]
    fn structured_transcript_artifact_rows_include_data_alias_and_schema_gated_preview() {
        let artifacts = vec![
            protocol::StructuredTranscriptArtifact {
                key: "agent_context".to_string(),
                path: "/tmp/.agent-context.json".to_string(),
                bytes: Some(123),
                schema: Some(protocol::AGENT_CONTEXT_SCHEMA_NAME.to_string()),
                record_count: None,
                inline_json: Some(
                    serde_json::json!({"schema": protocol::AGENT_CONTEXT_SCHEMA_NAME, "repo": {"name": "agent-hub"}}),
                ),
            },
            protocol::StructuredTranscriptArtifact {
                key: "run_json".to_string(),
                path: "/tmp/run.json".to_string(),
                bytes: Some(321),
                schema: Some(protocol::JOB_RUN_SCHEMA_NAME.to_string()),
                record_count: None,
                inline_json: Some(
                    serde_json::json!({"schema": protocol::JOB_RUN_SCHEMA_NAME, "workspace": "/workspace"}),
                ),
            },
            protocol::StructuredTranscriptArtifact {
                key: "job_events".to_string(),
                path: "/tmp/job-events.jsonl".to_string(),
                bytes: Some(456),
                schema: Some(protocol::JOB_EVENT_SCHEMA_NAME.to_string()),
                record_count: Some(10),
                inline_json: Some(serde_json::json!({"unexpected": true})),
            },
        ];

        let rows = structured_transcript_artifact_rows("build", &artifacts);
        assert_eq!(rows.len(), 3);

        let agent_context = rows
            .iter()
            .find(|row| row.key == "agent_context")
            .expect("agent_context row");
        assert_eq!(
            agent_context.context_alias,
            "job_artifact_paths.build.agent_context"
        );
        assert_eq!(
            agent_context.metadata_context_alias,
            "job_artifact_metadata.build.agent_context"
        );
        assert_eq!(
            agent_context.data_context_alias,
            "job_artifact_data.build.agent_context"
        );
        assert!(agent_context.has_inline_preview);
        assert!(
            agent_context
                .inline_preview
                .contains("\"schema\": \"agent_context_v1\"")
        );

        let run_json = rows
            .iter()
            .find(|row| row.key == "run_json")
            .expect("run_json row");
        assert_eq!(
            run_json.data_context_alias,
            "job_artifact_data.build.run_json"
        );
        assert!(run_json.has_inline_preview);
        assert!(
            run_json
                .inline_preview
                .contains("\"schema\": \"job_run_v1\"")
        );

        let job_events = rows
            .iter()
            .find(|row| row.key == "job_events")
            .expect("job_events row");
        assert_eq!(
            job_events.data_context_alias,
            "job_artifact_data.build.job_events"
        );
        assert!(job_events.has_inline_preview);
        assert!(job_events.inline_preview.contains("\"unexpected\": true"));
    }

    #[test]
    fn structured_inline_preview_is_schema_gated() {
        let inline = serde_json::json!({"schema": protocol::AGENT_CONTEXT_SCHEMA_NAME});
        assert!(
            structured_inline_preview(
                "agent_context",
                Some(protocol::AGENT_CONTEXT_SCHEMA_NAME),
                Some(&inline)
            )
            .is_some()
        );
        let run_inline = serde_json::json!({"schema": protocol::JOB_RUN_SCHEMA_NAME});
        assert!(
            structured_inline_preview(
                "run_json",
                Some(protocol::JOB_RUN_SCHEMA_NAME),
                Some(&run_inline)
            )
            .is_some()
        );
        assert!(
            structured_inline_preview("agent_context", Some("other_schema"), Some(&inline))
                .is_none()
        );
        assert!(
            structured_inline_preview("run_json", Some("other_schema"), Some(&run_inline))
                .is_none()
        );
        let transcript_inline = serde_json::json!(["{\"event\":\"ok\"}"]);
        assert!(
            structured_inline_preview("transcript_jsonl", None, Some(&transcript_inline)).is_some()
        );
        assert!(structured_inline_preview("transcript_jsonl", None, Some(&inline)).is_none());
        assert!(
            structured_inline_preview("job_events", Some("agent_context_v1"), Some(&inline))
                .is_none()
        );
        let job_events_inline = serde_json::json!([{"kind": "system", "message": "ok"}]);
        assert!(
            structured_inline_preview(
                "job_events",
                Some(protocol::JOB_EVENT_SCHEMA_NAME),
                Some(&job_events_inline)
            )
            .is_some()
        );
    }

    #[test]
    fn transcript_jsonl_preview_is_strictly_bounded() {
        let valid = serde_json::json!(["line-a", "line-b"]);
        assert!(structured_inline_preview("transcript_jsonl", None, Some(&valid)).is_some());

        let over_record_budget = serde_json::Value::Array(
            (0..(TRANSCRIPT_JSONL_PREVIEW_MAX_RECORDS + 1))
                .map(|idx| serde_json::Value::String(format!("line-{idx}")))
                .collect(),
        );
        assert!(
            structured_inline_preview("transcript_jsonl", None, Some(&over_record_budget))
                .is_none()
        );

        let over_line_budget =
            serde_json::json!(["x".repeat(TRANSCRIPT_JSONL_PREVIEW_MAX_LINE_CHARS + 1)]);
        assert!(
            structured_inline_preview("transcript_jsonl", None, Some(&over_line_budget)).is_none()
        );
    }

    #[test]
    fn truncate_preview_adds_suffix_when_value_exceeds_limit() {
        let value = "abcdef";
        let truncated = truncate_preview(value, 3);
        assert_eq!(truncated, "abc\n... (preview truncated)");
        assert_eq!(truncate_preview(value, 6), value);
    }

    #[test]
    fn work_item_structured_artifact_preview_rows_use_top_level_job_artifact_data() {
        let context = serde_json::json!({
            "job_artifact_data": {
                "build": {
                    "agent_context": {
                        "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME,
                        "repo": {
                            "name": "agent-hub"
                        }
                    },
                    "run_json": {
                        "schema": protocol::JOB_RUN_SCHEMA_NAME,
                        "workspace": "/workspace"
                    },
                    "job_events": {
                        "schema": protocol::JOB_EVENT_SCHEMA_NAME
                    }
                }
            },
            "job_artifact_metadata": {
                "build": {
                    "agent_context": {
                        "schema": "not_used_for_preview"
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_preview_rows(&context);
        assert_eq!(rows.len(), 3);

        let agent_context = rows
            .iter()
            .find(|row| row.artifact_key == "agent_context")
            .expect("agent_context row");
        assert_eq!(agent_context.step_id, "build");
        assert_eq!(
            agent_context.data_context_alias,
            "job_artifact_data.build.agent_context"
        );
        assert_eq!(
            agent_context.metadata_context_alias,
            "job_artifact_metadata.build.agent_context"
        );
        assert_eq!(agent_context.schema, protocol::AGENT_CONTEXT_SCHEMA_NAME);
        assert!(
            agent_context
                .inline_preview
                .contains("\"schema\": \"agent_context_v1\"")
        );

        let run_json = rows
            .iter()
            .find(|row| row.artifact_key == "run_json")
            .expect("run_json row");
        assert_eq!(run_json.schema, protocol::JOB_RUN_SCHEMA_NAME);
        assert!(
            run_json
                .inline_preview
                .contains("\"schema\": \"job_run_v1\"")
        );

        let job_events = rows
            .iter()
            .find(|row| row.artifact_key == "job_events")
            .expect("job_events row");
        assert_eq!(job_events.schema, protocol::JOB_EVENT_SCHEMA_NAME);
        assert!(
            job_events
                .inline_preview
                .contains("\"schema\": \"protocol::JobEvent\"")
        );
    }

    #[test]
    fn work_item_structured_artifact_preview_rows_can_fall_back_to_top_level_metadata_schema() {
        let context = serde_json::json!({
            "job_artifact_data": {
                "build": {
                    "run_json": {
                        "workspace": "/workspace"
                    }
                }
            },
            "job_artifact_metadata": {
                "build": {
                    "run_json": {
                        "schema": protocol::JOB_RUN_SCHEMA_NAME
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_preview_rows(&context);
        assert_eq!(rows.len(), 1);
        let run_json = rows.first().expect("run_json preview row");
        assert_eq!(run_json.artifact_key, "run_json");
        assert_eq!(run_json.schema, protocol::JOB_RUN_SCHEMA_NAME);
        assert!(
            run_json
                .inline_preview
                .contains("\"workspace\": \"/workspace\"")
        );
    }

    #[test]
    fn work_item_structured_artifact_preview_rows_include_child_result_aliases() {
        let context = serde_json::json!({
            "job_artifact_data": {
                "build": {
                    "run_json": {
                        "schema": protocol::JOB_RUN_SCHEMA_NAME,
                        "workspace": "/workspace"
                    }
                }
            },
                    "child_results": {
                "fanout": {
                    "job_artifact_data": {
                        "unit_test": {
                            "agent_context": {
                                "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME,
                                "repo": {
                                    "name": "agent-hub"
                                }
                            },
                            "transcript_jsonl": ["{\"event\":\"done\"}"]
                        }
                    },
                    "job_artifact_metadata": {
                        "unit_test": {
                            "agent_context": {
                                "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME
                            }
                        }
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_preview_rows(&context);
        assert_eq!(rows.len(), 3);

        let top_level = rows
            .iter()
            .find(|row| row.data_context_alias == "job_artifact_data.build.run_json")
            .expect("top-level run_json row");
        assert_eq!(top_level.step_id, "build");
        assert_eq!(top_level.schema, protocol::JOB_RUN_SCHEMA_NAME);

        let child = rows
            .iter()
            .find(|row| {
                row.data_context_alias
                    == "child_results.fanout.job_artifact_data.unit_test.agent_context"
            })
            .expect("child agent_context row");
        assert_eq!(child.step_id, "fanout.unit_test");
        assert_eq!(
            child.metadata_context_alias,
            "child_results.fanout.job_artifact_metadata.unit_test.agent_context"
        );
        assert_eq!(child.schema, protocol::AGENT_CONTEXT_SCHEMA_NAME);
        assert!(
            child
                .inline_preview
                .contains("\"schema\": \"agent_context_v1\"")
        );
        let child_transcript = rows
            .iter()
            .find(|row| {
                row.data_context_alias
                    == "child_results.fanout.job_artifact_data.unit_test.transcript_jsonl"
            })
            .expect("child transcript_jsonl row");
        assert_eq!(child_transcript.step_id, "fanout.unit_test");
        assert_eq!(child_transcript.schema, "unknown");
        assert!(child_transcript.inline_preview.contains("event"));
    }

    #[test]
    fn work_item_structured_artifact_preview_rows_child_previews_can_fall_back_to_metadata_schema()
    {
        let context = serde_json::json!({
            "child_results": {
                "fanout": {
                    "job_artifact_data": {
                        "unit_test": {
                            "run_json": {
                                "workspace": "/workspace/child"
                            }
                        }
                    },
                    "job_artifact_metadata": {
                        "unit_test": {
                            "run_json": {
                                "schema": protocol::JOB_RUN_SCHEMA_NAME
                            }
                        }
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_preview_rows(&context);
        assert_eq!(rows.len(), 1);
        let child_run_json = rows.first().expect("child run_json row");
        assert_eq!(child_run_json.step_id, "fanout.unit_test");
        assert_eq!(
            child_run_json.data_context_alias,
            "child_results.fanout.job_artifact_data.unit_test.run_json"
        );
        assert_eq!(
            child_run_json.metadata_context_alias,
            "child_results.fanout.job_artifact_metadata.unit_test.run_json"
        );
        assert_eq!(child_run_json.schema, protocol::JOB_RUN_SCHEMA_NAME);
        assert!(
            child_run_json
                .inline_preview
                .contains("\"workspace\": \"/workspace/child\"")
        );
    }

    #[test]
    fn work_item_structured_artifact_preview_rows_return_empty_without_job_artifact_data() {
        let context = serde_json::json!({
            "job_artifact_metadata": {
                "build": {
                    "agent_context": {
                        "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME
                    }
                }
            }
        });

        assert!(work_item_structured_artifact_preview_rows(&context).is_empty());
    }

    #[test]
    fn work_item_structured_value_preview_rows_include_top_level_and_child_namespaces() {
        let context = serde_json::json!({
            "job_outputs": {
                "build": {
                    "summary": "ok"
                }
            },
            "job_results": {
                "build": {
                    "outcome": "succeeded"
                }
            },
            "child_results": {
                "fanout": {
                    "job_outputs": {
                        "unit_test": {
                            "summary": "pass"
                        }
                    },
                    "job_results": {
                        "unit_test": {
                            "outcome": "succeeded"
                        }
                    },
                    "unrelated": {
                        "ignored": true
                    }
                }
            },
            "unrelated": {
                "should_not_show": true
            }
        });

        let rows = work_item_structured_value_preview_rows(&context);
        assert_eq!(rows.len(), 4);

        let top_output = rows
            .iter()
            .find(|row| row.context_alias == "job_outputs.build")
            .expect("job_outputs preview");
        assert_eq!(top_output.namespace, "job_outputs");
        assert_eq!(top_output.step_id, "build");
        assert_eq!(top_output.interpolation, "{{job_outputs.build}}");
        assert_eq!(top_output.value_kind, "object");
        assert!(top_output.inline_preview.contains("\"summary\": \"ok\""));

        let top_result = rows
            .iter()
            .find(|row| row.context_alias == "job_results.build")
            .expect("job_results preview");
        assert_eq!(top_result.namespace, "job_results");
        assert_eq!(top_result.interpolation, "{{job_results.build}}");

        let child_output = rows
            .iter()
            .find(|row| row.context_alias == "child_results.fanout.job_outputs.unit_test")
            .expect("child output preview");
        assert_eq!(child_output.namespace, "child_results.fanout.job_outputs");
        assert_eq!(child_output.step_id, "fanout.unit_test");
        assert_eq!(
            child_output.interpolation,
            "{{child_results.fanout.job_outputs.unit_test}}"
        );
        assert!(
            child_output
                .inline_preview
                .contains("\"summary\": \"pass\"")
        );

        let child_result = rows
            .iter()
            .find(|row| row.context_alias == "child_results.fanout.job_results.unit_test")
            .expect("child result preview");
        assert_eq!(child_result.namespace, "child_results.fanout.job_results");
        assert_eq!(child_result.step_id, "fanout.unit_test");
    }

    #[test]
    fn work_item_structured_value_preview_rows_are_bounded() {
        let context = serde_json::json!({
            "job_outputs": {
                "build": {
                    "message": "x".repeat(20_000)
                }
            }
        });

        let rows = work_item_structured_value_preview_rows(&context);
        assert_eq!(rows.len(), 1);
        let preview = &rows[0].inline_preview;
        assert!(preview.contains("... (preview truncated)"));
    }

    #[test]
    fn work_item_structured_value_preview_rows_omit_over_budget_values() {
        let context = serde_json::json!({
            "job_outputs": {
                "build": {
                    "blob": "x".repeat(STRUCTURED_VALUE_PREVIEW_MAX_JSON_BYTES + 1024)
                }
            }
        });

        let rows = work_item_structured_value_preview_rows(&context);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].inline_preview.contains("preview omitted"));
        assert!(rows[0].inline_preview.contains("exceeds"));
    }

    #[test]
    fn work_item_structured_artifact_projection_preview_rows_include_top_level_and_child_variants()
    {
        let context = serde_json::json!({
            "job_artifact_paths": {
                "build": {
                    "run_json": "/tmp/build/run.json"
                }
            },
            "job_artifact_metadata": {
                "build": {
                    "run_json": {
                        "schema": protocol::JOB_RUN_SCHEMA_NAME,
                        "path": "/tmp/build/run.json"
                    }
                }
            },
            "child_results": {
                "fanout": {
                    "job_artifact_paths": {
                        "unit_test": {
                            "agent_context": "/tmp/fanout/unit_test/.agent-context.json"
                        }
                    },
                    "job_artifact_metadata": {
                        "unit_test": {
                            "agent_context": {
                                "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME
                            }
                        }
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_projection_preview_rows(&context);
        assert_eq!(rows.len(), 4);

        let top_paths = rows
            .iter()
            .find(|row| row.context_alias == "job_artifact_paths.build")
            .expect("top-level paths row");
        assert_eq!(top_paths.namespace, "job_artifact_paths");
        assert_eq!(top_paths.interpolation, "{{job_artifact_paths.build}}");
        assert!(top_paths.inline_preview.contains("run_json"));

        let top_metadata = rows
            .iter()
            .find(|row| row.context_alias == "job_artifact_metadata.build")
            .expect("top-level metadata row");
        assert_eq!(top_metadata.namespace, "job_artifact_metadata");
        assert_eq!(
            top_metadata.interpolation,
            "{{job_artifact_metadata.build}}"
        );
        assert!(top_metadata.inline_preview.contains("job_run_v1"));

        let child_paths = rows
            .iter()
            .find(|row| row.context_alias == "child_results.fanout.job_artifact_paths.unit_test")
            .expect("child paths row");
        assert_eq!(
            child_paths.namespace,
            "child_results.fanout.job_artifact_paths"
        );
        assert_eq!(child_paths.step_id, "fanout.unit_test");
        assert_eq!(
            child_paths.interpolation,
            "{{child_results.fanout.job_artifact_paths.unit_test}}"
        );

        let child_metadata = rows
            .iter()
            .find(|row| row.context_alias == "child_results.fanout.job_artifact_metadata.unit_test")
            .expect("child metadata row");
        assert_eq!(
            child_metadata.namespace,
            "child_results.fanout.job_artifact_metadata"
        );
        assert_eq!(child_metadata.step_id, "fanout.unit_test");
        assert!(child_metadata.inline_preview.contains("agent_context_v1"));
    }

    #[test]
    fn work_item_structured_artifact_projection_preview_rows_are_bounded() {
        let context = serde_json::json!({
            "job_artifact_metadata": {
                "build": {
                    "run_json": {
                        "blob": "x".repeat(20_000)
                    }
                }
            }
        });

        let rows = work_item_structured_artifact_projection_preview_rows(&context);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].context_alias, "job_artifact_metadata.build");
        assert!(rows[0].inline_preview.contains("... (preview truncated)"));
    }

    #[test]
    fn job_detail_structured_value_preview_rows_use_current_job_result_aliases() {
        let result = protocol::JobResult {
            outcome: protocol::ExecutionOutcome::Succeeded,
            termination_reason: protocol::TerminationReason::ExitCode,
            exit_code: Some(0),
            timing: protocol::ExecutionTiming {
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                duration_ms: 123,
            },
            artifacts: protocol::ArtifactSummary {
                stdout_path: "/tmp/stdout.log".to_string(),
                stderr_path: "/tmp/stderr.log".to_string(),
                output_path: Some("/tmp/output.json".to_string()),
                transcript_dir: Some("/tmp".to_string()),
                stdout_bytes: Some(1),
                stderr_bytes: Some(1),
                output_bytes: Some(1),
                structured_transcript_artifacts: Vec::new(),
            },
            output_json: Some(serde_json::json!({"summary": "ok"})),
            repo: None,
            github_pr: None,
            error_message: None,
            cancellation_acknowledgement: None,
        };

        let rows = job_detail_structured_value_preview_rows("build", Some(&result));
        assert_eq!(rows.len(), 2);

        let output_row = rows
            .iter()
            .find(|row| row.context_alias == "job_outputs.build")
            .expect("job_outputs row");
        assert_eq!(output_row.namespace, "job_outputs");
        assert_eq!(output_row.interpolation, "{{job_outputs.build}}");
        assert_eq!(output_row.step_id, "build");
        assert!(output_row.inline_preview.contains("\"summary\": \"ok\""));

        let result_row = rows
            .iter()
            .find(|row| row.context_alias == "job_results.build")
            .expect("job_results row");
        assert_eq!(result_row.namespace, "job_results");
        assert_eq!(result_row.interpolation, "{{job_results.build}}");
        assert_eq!(result_row.step_id, "build");
        assert!(
            result_row
                .inline_preview
                .contains("\"outcome\": \"succeeded\"")
        );
        assert!(result_row.inline_preview.contains("\"output_json\""));
    }

    #[test]
    fn job_detail_structured_value_preview_rows_skip_job_outputs_when_missing() {
        let result = protocol::JobResult {
            outcome: protocol::ExecutionOutcome::Succeeded,
            termination_reason: protocol::TerminationReason::ExitCode,
            exit_code: Some(0),
            timing: protocol::ExecutionTiming {
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                duration_ms: 123,
            },
            artifacts: protocol::ArtifactSummary {
                stdout_path: "/tmp/stdout.log".to_string(),
                stderr_path: "/tmp/stderr.log".to_string(),
                output_path: Some("/tmp/output.json".to_string()),
                transcript_dir: Some("/tmp".to_string()),
                stdout_bytes: Some(1),
                stderr_bytes: Some(1),
                output_bytes: Some(1),
                structured_transcript_artifacts: Vec::new(),
            },
            output_json: None,
            repo: None,
            github_pr: None,
            error_message: None,
            cancellation_acknowledgement: None,
        };

        let rows = job_detail_structured_value_preview_rows("build", Some(&result));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].context_alias, "job_results.build");
    }

    #[test]
    fn conventional_artifact_rows_use_projected_aliases_and_sparse_values() {
        let artifacts = protocol::ArtifactSummary {
            stdout_path: "/tmp/job/stdout.log".to_string(),
            stderr_path: "/tmp/job/stderr.log".to_string(),
            output_path: Some("  ".to_string()),
            transcript_dir: Some("/tmp/job".to_string()),
            stdout_bytes: Some(42),
            stderr_bytes: Some(21),
            output_bytes: Some(9),
            structured_transcript_artifacts: Vec::new(),
        };

        let rows = conventional_artifact_rows("build", &artifacts);
        assert_eq!(rows.len(), 3);

        let stdout = rows
            .iter()
            .find(|row| row.key == "stdout_log")
            .expect("stdout row");
        assert_eq!(stdout.context_alias, "job_artifact_paths.build.stdout_log");
        assert_eq!(
            stdout.metadata_context_alias,
            "job_artifact_metadata.build.stdout_log"
        );
        assert_eq!(stdout.path, "/tmp/job/stdout.log");
        assert_eq!(stdout.bytes, "42");

        let transcript_dir = rows
            .iter()
            .find(|row| row.key == "transcript_dir")
            .expect("transcript_dir row");
        assert_eq!(transcript_dir.path, "/tmp/job");
        assert_eq!(transcript_dir.bytes, "unknown");

        assert!(rows.iter().all(|row| row.key != "output_json"));
    }

    #[test]
    fn orchestration_job_detail_template_renders_conventional_artifact_section() {
        let html = OrchestrationJobDetailTemplate {
            email: "".to_string(),
            job: JobRow {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "build".to_string(),
                state: "succeeded".to_string(),
                assigned_worker_id: "w1".to_string(),
                updated_at: "2026-01-01 00:00:00 UTC".to_string(),
            },
            structured_value_previews: Vec::new(),
            conventional_artifacts: vec![super::ConventionalArtifactRow {
                key: "stdout_log".to_string(),
                path: "/tmp/job/stdout.log".to_string(),
                bytes: "42".to_string(),
                context_alias: "job_artifact_paths.build.stdout_log".to_string(),
                metadata_context_alias: "job_artifact_metadata.build.stdout_log".to_string(),
            }],
            structured_transcript_artifacts: Vec::new(),
            retrievable_artifacts: vec![super::RetrievableArtifactRow {
                key: "stdout_log".to_string(),
                path: "/tmp/job/stdout.log".to_string(),
                content_type: "text/plain; charset=utf-8".to_string(),
                open_href: "/orchestration/jobs/123/artifacts/stdout_log".to_string(),
                download_href: "/orchestration/jobs/123/artifacts/stdout_log?download=1"
                    .to_string(),
            }],
            has_auth: false,
        }
        .render()
        .expect("render orchestration job detail");

        assert!(html.contains("Conventional projected artifacts"));
        assert!(html.contains("job_artifact_paths.build.stdout_log"));
        assert!(html.contains("job_artifact_metadata.build.stdout_log"));
        assert!(html.contains("/tmp/job/stdout.log"));
        assert!(html.contains("Artifact retrieval"));
        assert!(html.contains("/orchestration/jobs/123/artifacts/stdout_log"));
        assert!(html.contains("download=1"));
    }

    #[test]
    fn resolve_job_artifact_path_uses_persisted_metadata_only() {
        let result = protocol::JobResult {
            outcome: protocol::ExecutionOutcome::Succeeded,
            termination_reason: protocol::TerminationReason::ExitCode,
            exit_code: Some(0),
            timing: protocol::ExecutionTiming {
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                duration_ms: 12,
            },
            artifacts: protocol::ArtifactSummary {
                stdout_path: " /tmp/stdout.log ".to_string(),
                stderr_path: "/tmp/stderr.log".to_string(),
                output_path: Some("/tmp/output.json".to_string()),
                transcript_dir: Some("/tmp/transcript".to_string()),
                stdout_bytes: Some(1),
                stderr_bytes: Some(2),
                output_bytes: Some(3),
                structured_transcript_artifacts: vec![
                    protocol::StructuredTranscriptArtifact {
                        key: "agent_context".to_string(),
                        path: "/tmp/.agent-context.json".to_string(),
                        bytes: Some(4),
                        schema: Some(protocol::AGENT_CONTEXT_SCHEMA_NAME.to_string()),
                        record_count: None,
                        inline_json: None,
                    },
                    protocol::StructuredTranscriptArtifact {
                        key: "job_events".to_string(),
                        path: "/tmp/job-events.jsonl".to_string(),
                        bytes: Some(5),
                        schema: Some(protocol::JOB_EVENT_SCHEMA_NAME.to_string()),
                        record_count: Some(6),
                        inline_json: None,
                    },
                ],
            },
            output_json: None,
            repo: None,
            github_pr: None,
            error_message: None,
            cancellation_acknowledgement: None,
        };

        assert_eq!(
            resolve_job_artifact_path(&result, "stdout_log").expect("stdout path"),
            "/tmp/stdout.log"
        );
        assert_eq!(
            resolve_job_artifact_path(&result, "agent_context").expect("agent_context path"),
            "/tmp/.agent-context.json"
        );
        assert!(resolve_job_artifact_path(&result, "run_json").is_err());
        assert!(resolve_job_artifact_path(&result, "transcript_dir").is_err());
        assert!(resolve_job_artifact_path_in_store(&result, "stdout_log", "/artifacts").is_err());

        let mut relative_result = result.clone();
        relative_result.artifacts.stdout_path = "transcripts/job-1/stdout.log".to_string();
        assert_eq!(
            resolve_job_artifact_path_in_store(&relative_result, "stdout_log", "/artifacts")
                .expect("resolve relative in store"),
            std::path::PathBuf::from("/artifacts/transcripts/job-1/stdout.log")
        );
    }

    #[test]
    fn retrievable_artifact_rows_include_only_known_available_keys() {
        let job_id = uuid::Uuid::new_v4();
        let result = protocol::JobResult {
            outcome: protocol::ExecutionOutcome::Succeeded,
            termination_reason: protocol::TerminationReason::ExitCode,
            exit_code: Some(0),
            timing: protocol::ExecutionTiming {
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                duration_ms: 12,
            },
            artifacts: protocol::ArtifactSummary {
                stdout_path: "/tmp/stdout.log".to_string(),
                stderr_path: "/tmp/stderr.log".to_string(),
                output_path: None,
                transcript_dir: Some("/tmp/transcript".to_string()),
                stdout_bytes: Some(1),
                stderr_bytes: Some(2),
                output_bytes: None,
                structured_transcript_artifacts: vec![protocol::StructuredTranscriptArtifact {
                    key: "transcript_jsonl".to_string(),
                    path: "/tmp/transcript.jsonl".to_string(),
                    bytes: Some(10),
                    schema: None,
                    record_count: Some(2),
                    inline_json: None,
                }],
            },
            output_json: None,
            repo: None,
            github_pr: None,
            error_message: None,
            cancellation_acknowledgement: None,
        };

        let rows = retrievable_artifact_rows_for_job(job_id, &result);
        let keys = rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>();
        assert_eq!(keys, vec!["stdout_log", "stderr_log", "transcript_jsonl"]);
        assert!(
            rows.iter()
                .all(|row| row.open_href.contains(&job_id.to_string()))
        );
        assert!(
            rows.iter()
                .all(|row| row.download_href.contains("download=1"))
        );
        assert!(rows.iter().all(|row| !row.key.contains("transcript_dir")));
    }

    #[test]
    fn sanitize_content_disposition_filename_replaces_control_chars() {
        let sanitized = sanitize_content_disposition_filename("bad\0name\n\x7f.txt");
        assert_eq!(sanitized, "bad_name__.txt");
    }
}

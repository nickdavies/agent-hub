pub mod approvals;
pub mod artifact_store;
pub mod auth;
pub mod config;
pub mod environment;
pub mod github_poller;
pub mod hooks;
pub mod job_events;
pub mod notifier;
pub mod oauth;
pub mod orchestration;
pub mod presence;
pub mod pushover;
pub mod questions;
pub mod repo_discovery;
pub mod scm;
pub mod session_views;
pub(crate) use session_views::resolve_effective_status;
pub mod sessions;
pub mod storage;
pub mod task_adapter;
pub mod task_files;
#[allow(dead_code)]
pub mod task_vikunja;
pub mod web;
pub mod webhook;
pub mod workflows;

use std::sync::Arc;
use std::time::Duration;

use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::RwLock;
use tower_sessions::{MemoryStore, SessionManagerLayer};
use uuid::Uuid;

use crate::error::AppError;
use crate::mcp;
use approvals::{ApprovalRegistry, ApprovalStatus};
use artifact_store::{ArtifactStore, DirectoryStore};
use config::{
    ApprovalFeatureMode, AuthMode, LoadedConfigTree, NotifyConfig, NotifyConfigUpdate,
    ServerConfig, SharedNotifyConfig,
};
use github_poller::GitHubPoller;
use hooks::PendingNotifications;
use job_events::JobEventHub;
use notifier::Notifier;
use oauth::OAuthManager;
use orchestration::OrchestrationService;
use presence::{Presence, PresenceUpdate};
use questions::QuestionRegistry;
use repo_discovery::discover_repositories_from_config;
use session_views::build_session_views;
use sessions::{SessionConfigUpdate, SessionRegistry};
use task_adapter::TaskAdapter;
use workflows::WorkflowCatalog;

// Import protocol types used directly in this module's handlers.
use protocol::{
    ApprovalDecision, ApprovalModeResponse, ApprovalResolveRequest, ApprovalWaitResponse,
    ConfigResponse, QuestionDecision, QuestionResolveRequest, QuestionWaitResponse, SessionId,
};

pub(super) const DEFAULT_WEB_HOME: &str = "/orchestration";

pub struct AppState<N: Notifier> {
    pub config: Arc<ServerConfig>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub presence: Arc<Presence>,
    pub sessions: Arc<SessionRegistry>,
    pub notifier: Arc<N>,
    pub notify_config: SharedNotifyConfig,
    pub pending: Arc<PendingNotifications>,
    pub approvals: Arc<ApprovalRegistry>,
    pub questions: Arc<QuestionRegistry>,
    pub orchestration: Arc<OrchestrationService>,
    pub job_events: Arc<JobEventHub>,
    pub oauth: Arc<Option<OAuthManager>>,
    pub loaded_config_tree: Arc<Option<LoadedConfigTree>>,
}

impl<N: Notifier> Clone for AppState<N> {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            artifact_store: Arc::clone(&self.artifact_store),
            presence: Arc::clone(&self.presence),
            sessions: Arc::clone(&self.sessions),
            notifier: Arc::clone(&self.notifier),
            notify_config: Arc::clone(&self.notify_config),
            pending: Arc::clone(&self.pending),
            approvals: Arc::clone(&self.approvals),
            questions: Arc::clone(&self.questions),
            orchestration: Arc::clone(&self.orchestration),
            job_events: Arc::clone(&self.job_events),
            oauth: Arc::clone(&self.oauth),
            loaded_config_tree: Arc::clone(&self.loaded_config_tree),
        }
    }
}

pub fn router<N: Notifier>(state: AppState<N>) -> Router {
    let mcp_service = mcp::service(
        Arc::clone(&state.sessions),
        Arc::clone(&state.approvals),
        Arc::clone(&state.notify_config),
        Arc::clone(&state.presence),
    );

    let mut api_v1 = Router::new()
        .route("/hooks/stop", post(hooks::stop::<N>))
        .route("/hooks/notification", post(hooks::notification::<N>))
        .route("/hooks/session-end", post(hooks::session_end::<N>))
        .route("/hooks/status", post(hooks::status::<N>))
        .route("/presence", post(handle_presence_update::<N>))
        .route("/sessions", get(handle_list_sessions::<N>))
        .route("/sessions/{id}", put(handle_update_session::<N>))
        .route(
            "/orchestration/work-items",
            get(orchestration::handlers::list_work_items::<N>)
                .post(orchestration::handlers::create_work_item::<N>),
        )
        .route(
            "/orchestration/workflows",
            get(orchestration::handlers::list_workflows::<N>),
        )
        .route(
            "/orchestration/workflows/validate",
            get(orchestration::handlers::validate_workflows::<N>),
        )
        .route(
            "/orchestration/github/webhook",
            post(orchestration::webhook::github_webhook::<N>),
        )
        .route(
            "/orchestration/work-items/{id}/events",
            post(orchestration::handlers::fire_work_item_event::<N>),
        )
        .route(
            "/orchestration/work-items/{id}/event",
            post(orchestration::handlers::fire_work_item_event::<N>),
        )
        .route(
            "/orchestration/jobs",
            get(orchestration::handlers::list_jobs::<N>),
        )
        .route(
            "/orchestration/jobs/{id}",
            get(orchestration::handlers::get_job::<N>)
                .put(orchestration::handlers::update_job::<N>),
        )
        .route(
            "/orchestration/jobs/{id}/events",
            get(orchestration::handlers::job_events_sse::<N>),
        )
        .route(
            "/orchestration/jobs/{id}/cancellation-command",
            get(orchestration::handlers::get_job_cancellation_command::<N>),
        )
        .route(
            "/orchestration/workers",
            get(orchestration::handlers::list_workers::<N>)
                .post(orchestration::handlers::register_worker::<N>),
        )
        .route(
            "/orchestration/workers/{id}/heartbeat",
            post(orchestration::handlers::heartbeat_worker::<N>),
        )
        .route(
            "/orchestration/poll",
            post(orchestration::handlers::poll_job::<N>),
        )
        .route(
            "/orchestration/dispatch",
            post(orchestration::handlers::dispatch::<N>),
        )
        .route(
            "/config",
            get(handle_get_config::<N>).put(handle_put_config::<N>),
        )
        .nest_service("/mcp", mcp_service);

    // Mount approval API routes only when approval mode is not disabled
    if state.config.approval_mode != ApprovalFeatureMode::Disabled {
        api_v1 = api_v1
            .route("/hooks/approval", post(hooks::approval::<N>))
            .route("/hooks/question", post(hooks::question::<N>))
            .route("/approvals/pending", get(handle_list_pending::<N>))
            .route("/approvals/{id}", get(handle_get_approval::<N>))
            .route("/approvals/{id}/wait", get(handle_approval_wait::<N>))
            .route(
                "/approvals/{id}/resolve",
                post(handle_approval_resolve::<N>),
            )
            .route("/questions/pending", get(handle_list_questions::<N>))
            .route("/questions/{id}", get(handle_get_question::<N>))
            .route("/questions/{id}/wait", get(handle_question_wait::<N>))
            .route(
                "/questions/{id}/resolve",
                post(handle_question_resolve::<N>),
            )
            .route(
                "/sessions/{id}/approval-mode",
                get(handle_get_approval_mode::<N>),
            );
    }

    let api_v1 = if state.config.auth_mode == AuthMode::None {
        api_v1.with_state(state.clone())
    } else {
        api_v1
            .layer(from_fn_with_state(state.clone(), auth::require_auth::<N>))
            .with_state(state.clone())
    };

    let public = Router::new().route("/health", get(health));

    let mut app = Router::new().nest("/api/v1", api_v1).merge(public);

    // Redirect root to the orchestration shell
    app = app.route(
        "/",
        get(|| async { axum::response::Redirect::permanent(DEFAULT_WEB_HOME) }),
    );

    // Mount orchestration web UI routes.
    let mut web_routes = Router::new()
        .route("/orchestration", get(web::orchestration_dashboard::<N>))
        .route(
            "/orchestration/work-items",
            get(web::orchestration_work_items::<N>),
        )
        .route("/orchestration/jobs", get(web::orchestration_jobs::<N>))
        .route(
            "/orchestration/transcripts",
            get(web::orchestration_transcripts::<N>),
        )
        .route(
            "/orchestration/workers",
            get(web::orchestration_workers::<N>),
        )
        .route(
            "/orchestration/work-items/{id}",
            get(web::orchestration_work_item_detail::<N>),
        )
        .route(
            "/orchestration/jobs/{id}",
            get(web::orchestration_job_detail::<N>),
        )
        .route(
            "/orchestration/jobs/{id}/artifacts/{key}",
            get(web::orchestration_job_artifact::<N>),
        )
        .route(
            "/orchestration/work-items/{id}/action",
            post(web::orchestration_fire_work_item_action::<N>),
        );

    // Keep approvals web UI available when approval mode is enabled.
    if state.config.approval_mode != ApprovalFeatureMode::Disabled {
        web_routes = web_routes
            .route("/approvals", get(web::dashboard::<N>))
            .route("/approvals/queue", get(web::approval_queue::<N>))
            .route("/approvals/{id}", get(web::approval_detail::<N>));
    }

    if state.config.auth_mode != AuthMode::None {
        // Auth routes (public, no auth required)
        let auth_routes = Router::new()
            .route("/auth/login", get(web::login_page::<N>))
            .route("/auth/login/basic", post(web::basic_auth_login::<N>))
            .route("/auth/start/{provider}", get(oauth::start_auth::<N>))
            .route("/auth/callback/{provider}", get(oauth::callback::<N>))
            .route("/auth/logout", post(oauth::logout))
            .with_state(state.clone());

        web_routes = web_routes.layer(from_fn(auth::require_web_auth));
        app = app.merge(auth_routes);
    }

    app = app.merge(web_routes.with_state(state.clone()));

    // Session layer for OAuth (in-memory store, sessions lost on restart)
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store);

    app.layer(session_layer)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}

async fn handle_presence_update<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    Json(body): Json<PresenceUpdate>,
) -> axum::http::StatusCode {
    state.presence.set(body.state).await;
    tracing::info!(state = ?body.state, "presence updated");
    axum::http::StatusCode::OK
}

async fn handle_list_sessions<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
) -> Json<Vec<sessions::SessionView>> {
    Json(build_session_views(&state).await)
}

async fn handle_update_session<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(update): Json<SessionConfigUpdate>,
) -> Result<Json<sessions::SessionNotifyConfig>, crate::error::AppError> {
    let session_id = SessionId::new(id);
    state
        .sessions
        .update_config(&session_id, &update)
        .await
        .ok_or(crate::error::AppError::SessionNotFound(
            session_id.to_string(),
        ))
        .map(Json)
}

async fn handle_get_config<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
) -> Json<ConfigResponse> {
    let notify = state.notify_config.read().await.clone();
    let presence = state.presence.get().await;
    Json(ConfigResponse { notify, presence })
}

async fn handle_put_config<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    Json(update): Json<NotifyConfigUpdate>,
) -> Json<NotifyConfig> {
    let mut cfg = state.notify_config.write().await;
    cfg.apply(update);
    Json(cfg.clone())
}

// --- Approval API handlers ---

/// GET /api/v1/approvals/pending — list all pending approvals.
async fn handle_list_pending<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
) -> Json<Vec<approvals::Approval>> {
    Json(state.approvals.list_pending().await)
}

/// GET /api/v1/approvals/{id} — get a single approval by ID.
async fn handle_get_approval<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<approvals::Approval>, AppError> {
    state
        .approvals
        .get(id)
        .await
        .ok_or_else(|| AppError::ApprovalNotFound(id.to_string()))
        .map(Json)
}

/// GET /api/v1/approvals/{id}/wait — long-poll for approval decision (55s timeout).
async fn handle_approval_wait<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<(axum::http::StatusCode, Json<ApprovalWaitResponse>), AppError> {
    let mut rx = state
        .approvals
        .subscribe(id)
        .await
        .ok_or_else(|| AppError::ApprovalNotFound(id.to_string()))?;

    // Record that the gateway is actively polling for this approval.
    state.approvals.touch(id).await;

    // If already resolved, return immediately
    if rx.borrow().is_resolved() {
        let status = rx.borrow().clone();
        return Ok((
            axum::http::StatusCode::OK,
            Json(ApprovalWaitResponse { status }),
        ));
    }

    // Long-poll: wait up to 55s for a change
    let result = tokio::time::timeout(Duration::from_secs(55), rx.changed()).await;

    let status = rx.borrow().clone();
    if result.is_ok() && status.is_resolved() {
        Ok((
            axum::http::StatusCode::OK,
            Json(ApprovalWaitResponse { status }),
        ))
    } else {
        // Timeout or still pending
        Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(ApprovalWaitResponse {
                status: ApprovalStatus::Pending,
            }),
        ))
    }
}

/// POST /api/v1/approvals/{id}/resolve — approve/deny/cancel an approval.
async fn handle_approval_resolve<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<ApprovalResolveRequest>,
) -> Result<Json<approvals::Approval>, AppError> {
    let new_status = match req.decision {
        ApprovalDecision::Approve => ApprovalStatus::Approved {
            message: req.message,
        },
        ApprovalDecision::Deny => ApprovalStatus::Denied {
            reason: req.message.unwrap_or_default(),
        },
        ApprovalDecision::Cancel => ApprovalStatus::Cancelled,
    };

    state
        .approvals
        .resolve(id, new_status)
        .await
        .ok_or_else(|| AppError::ApprovalNotFound(id.to_string()))
        .map(Json)
}

/// GET /api/v1/sessions/{id}/approval-mode
async fn handle_get_approval_mode<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<ApprovalModeResponse>, AppError> {
    let session_id = SessionId::new(&id);
    let cfg = state
        .sessions
        .get_config(&session_id)
        .await
        .ok_or(AppError::SessionNotFound(id))?;
    Ok(Json(ApprovalModeResponse {
        approval_mode: cfg.approval_mode,
    }))
}

// --- Question API handlers ---

/// GET /api/v1/questions/pending — list all pending questions.
async fn handle_list_questions<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
) -> Json<Vec<questions::PendingQuestion>> {
    Json(state.questions.list_pending().await)
}

/// GET /api/v1/questions/{id} — get a single question by ID.
async fn handle_get_question<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<questions::PendingQuestion>, AppError> {
    state
        .questions
        .get(id)
        .await
        .ok_or_else(|| AppError::QuestionNotFound(id.to_string()))
        .map(Json)
}

/// GET /api/v1/questions/{id}/wait — long-poll for question answer (55s timeout).
async fn handle_question_wait<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<(axum::http::StatusCode, Json<QuestionWaitResponse>), AppError> {
    let mut rx = state
        .questions
        .subscribe(id)
        .await
        .ok_or_else(|| AppError::QuestionNotFound(id.to_string()))?;

    state.questions.touch(id).await;

    if rx.borrow().is_resolved() {
        let status = rx.borrow().clone();
        return Ok((
            axum::http::StatusCode::OK,
            Json(QuestionWaitResponse { status }),
        ));
    }

    let result = tokio::time::timeout(Duration::from_secs(55), rx.changed()).await;

    let status = rx.borrow().clone();
    if result.is_ok() && status.is_resolved() {
        Ok((
            axum::http::StatusCode::OK,
            Json(QuestionWaitResponse { status }),
        ))
    } else {
        Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(QuestionWaitResponse {
                status: questions::QuestionStatus::Pending,
            }),
        ))
    }
}

/// POST /api/v1/questions/{id}/resolve — answer/reject/cancel a question.
async fn handle_question_resolve<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(req): Json<QuestionResolveRequest>,
) -> Result<Json<questions::PendingQuestion>, AppError> {
    let new_status = match req.decision {
        QuestionDecision::Answer => questions::QuestionStatus::Answered {
            answers: req.answers.unwrap_or_default(),
        },
        QuestionDecision::Reject => questions::QuestionStatus::Rejected { reason: req.reason },
        QuestionDecision::Cancel => questions::QuestionStatus::Cancelled,
    };

    state
        .questions
        .resolve(id, new_status)
        .await
        .ok_or_else(|| AppError::QuestionNotFound(id.to_string()))
        .map(Json)
}

impl<N: Notifier> AppState<N> {
    pub fn new(server_config: ServerConfig, notifier: N, oauth: Option<OAuthManager>) -> Self {
        let artifact_store: Arc<dyn ArtifactStore> = Arc::new(DirectoryStore::new(
            std::path::PathBuf::from(server_config.artifact_store_root.clone()),
        ));
        let presence = Presence::new(server_config.presence_ttl_secs);
        let sessions = SessionRegistry::new(server_config.session_ttl_secs)
            .with_default_approval_mode(server_config.default_approval_mode);
        let notify_config = NotifyConfig::with_delay(server_config.notification_delay_secs);
        let loaded = config::load_config_tree(std::path::Path::new(&server_config.config_root))
            .unwrap_or_else(|error| {
                panic!(
                    "failed to load config tree from CONFIG_ROOT ({}): {error:?}",
                    server_config.config_root
                )
            });
        let system_operator_login = loaded.system.operator.as_str().trim().to_string();
        let system_operator_login =
            (!system_operator_login.is_empty()).then_some(system_operator_login);
        // WORKFLOWS_DIR is an independently supplied runtime catalog, not an
        // authoritative CONFIG_ROOT deployment input. It is parsed and validated
        // as one catalog before orchestration accepts work.
        let installed_workflows = loaded
            .system
            .workflows
            .as_ref()
            .map(|workflows| workflows.installed.as_slice());
        let workflows = Arc::new(
            WorkflowCatalog::load_from_dir_with_installed(
                &server_config.workflows_dir,
                installed_workflows,
            )
            .unwrap_or_else(|error| panic!("failed to load installed workflows: {error:#}")),
        );
        let task_adapter: Arc<dyn TaskAdapter> = match server_config.task_adapter.as_str() {
            "file" => {
                let store = task_files::TaskFileStore::new_with_state_dir(
                    server_config.task_dir.clone(),
                    server_config.task_state_dir.clone(),
                );
                store.ensure_dir().expect("create task dir");
                Arc::new(store)
            }
            other => panic!("unsupported TASK_ADAPTER '{other}'; only exact 'file' is allowed"),
        };
        let orchestration_db_path = if server_config.orchestration_db_path == ":memory:" {
            std::env::temp_dir()
                .join(format!(
                    "orchestration-test-{}.sqlite3",
                    uuid::Uuid::new_v4().simple()
                ))
                .to_string_lossy()
                .into_owned()
        } else {
            server_config.orchestration_db_path.clone()
        };
        // Build the required host-specific read-only observation registry.
        let scm_registry = scm::ScmHostRegistry::from_configs(&loaded.system.scm_hosts)
            .unwrap_or_else(|error| panic!("invalid [[system.scm_hosts]] config: {error}"));
        let github_poller = if server_config.github_polling_enabled && !scm_registry.is_empty() {
            Some(Arc::new(GitHubPoller::new(scm_registry.clone())))
        } else {
            None
        };
        let environments = loaded
            .environments
            .iter()
            .map(|environment| environment.to_definition())
            .collect();
        let overlays = loaded.overlays.clone();
        let triage_prompt = loaded.triage_prompt.clone();
        let runner_profiles = loaded.runner_profiles.clone();
        let environment_snapshots = loaded
            .environments
            .iter()
            .map(|environment| {
                let bundle = loaded
                    .environment_bundles
                    .get(&environment.toml.name)
                    .unwrap_or_else(|| {
                        panic!(
                            "validated environment bundle '{}' is unavailable",
                            environment.toml.name
                        )
                    });
                let mut policies = Vec::new();
                let mut policy_name = Some(environment.toml.network_policy.as_str());
                let mut visited = std::collections::HashSet::new();
                while let Some(name) = policy_name {
                    if !visited.insert(name.to_string()) {
                        panic!("network policy inheritance cycle at '{name}'");
                    }
                    let policy = loaded
                        .network_policies
                        .iter()
                        .find(|policy| policy.name == name)
                        .unwrap_or_else(|| panic!("environment references missing policy '{name}'"));
                    policies.push(policy.clone());
                    policy_name = policy.extends.as_deref();
                }
                (
                    environment.toml.name.clone(),
                    serde_json::json!({
                        "environment": environment.toml,
                        "network_policies": &policies,
                        "bundle_digest": bundle.digest,
                        "network_policy_bundle_digests": policies.iter().map(|policy| {
                            let digests = loaded
                                .network_policy_file_digests
                                .get(&policy.name)
                                .unwrap_or_else(|| panic!("validated digest for network policy '{}' is unavailable", policy.name))
                                .clone();
                            (policy.name.clone(), digests)
                        }).collect::<std::collections::BTreeMap<_, _>>(),
                    }),
                )
            })
            .collect();
        let discovered_repos = discover_repositories_from_config(
            loaded.system.repo_discovery.as_ref(),
            &loaded.triage_mounts,
        )
        .unwrap_or_else(|error| {
            panic!("failed to discover repositories from system.repo_discovery: {error:#}")
        });
        let loaded_config_tree = Some(loaded);
        let orchestration = Arc::new(
            OrchestrationService::new_with_github_poller_and_interval(
                Arc::clone(&workflows),
                task_adapter,
                orchestration_db_path,
                github_poller,
                server_config.github_poll_interval_secs,
            )
            .with_environments(environments)
            .with_overlays(overlays)
            .with_discovered_repos(discovered_repos)
            .with_operator_login(system_operator_login.clone())
            .with_runner_profiles(runner_profiles)
            .with_scm_hosts(scm_registry)
            .with_environment_snapshots(environment_snapshots)
            .with_triage_prompt(triage_prompt)
            .with_server_approval_mode(server_config.approval_mode),
        );
        let job_events = Arc::new(JobEventHub::new());

        Self {
            config: Arc::new(server_config),
            artifact_store,
            presence: Arc::new(presence),
            sessions: Arc::new(sessions),
            notifier: Arc::new(notifier),
            notify_config: Arc::new(RwLock::new(notify_config)),
            pending: Arc::new(PendingNotifications::new()),
            approvals: Arc::new(ApprovalRegistry::new()),
            questions: Arc::new(QuestionRegistry::new()),
            orchestration,
            job_events,
            oauth: Arc::new(oauth),
            loaded_config_tree: Arc::new(loaded_config_tree),
        }
    }

    /// Capture current state for persistence.
    pub async fn snapshot(&self) -> storage::PersistedState {
        storage::PersistedState {
            sessions: self.sessions.snapshot().await,
            notify_config: Some(self.notify_config.read().await.clone()),
            presence: Some(self.presence.raw_state().await),
            pending_approvals: self.approvals.snapshot().await,
        }
    }

    /// Restore state from a persisted snapshot.
    pub async fn restore(&self, state: storage::PersistedState) {
        self.sessions.restore(state.sessions).await;
        if let Some(cfg) = state.notify_config {
            *self.notify_config.write().await = cfg;
        }
        if let Some(presence) = state.presence {
            self.presence.set(presence).await;
        }
        if !state.pending_approvals.is_empty() {
            self.approvals.restore(state.pending_approvals).await;
        }
    }
}

// ===================================================================
// Integration tests — exercises the full HTTP API stack (Axum routing,
// serde extraction, handler logic, response serialization) without
// needing a running server, gateway, or opencode instance.
// ===================================================================

#[cfg(test)]
mod integration_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header;
    use axum::http::{Request, StatusCode as AxumStatus};
    use config::{ApprovalFeatureMode, AuthMode, ServerConfig, Token};
    use notifier::NullNotifier;
    use serde_json::Value;
    use sessions::SessionApprovalMode;
    use tower::ServiceExt; // for oneshot

    /// Build a minimal hermetic config root for `AppState::new`. The integration
    /// tests don't exercise environments, overlays, network policies, images, or
    /// triage prompt loading, so we just need a `system.toml` that satisfies the
    /// `installed = []` invariants `load_config_tree` enforces.
    fn make_hermetic_config_root() -> std::path::PathBuf {
        unsafe { std::env::set_var("SCM_TEST_INTEGRATION_READ_TOKEN", "read-token") };
        let root =
            std::env::temp_dir().join(format!("ah-test-config-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).expect("create hermetic config root");
        let body = r#"[system]
operator = "test-operator"
bot_identity = "test-bot"

[system.images]
installed = []

[system.network_policies]
installed = []

[system.environments]
installed = []
triage_environment = "test-triage"

[[system.scm_hosts]]
kind = "github"
host = "github.com"
api_base_url = "https://api.github.com"
read_token_env = "SCM_TEST_INTEGRATION_READ_TOKEN"
"#;
        std::fs::write(root.join("system.toml"), body).expect("write hermetic system.toml");
        std::fs::write(
            root.join("local-agent.json"),
            r#"{
                "default_agent": {
                    "kind": "generic",
                    "command": "true",
                    "args": [],
                    "container_image": "local/agent-hub-test:latest"
                }
            }"#,
        )
        .expect("write hermetic local-agent config");
        root
    }

    /// Build a fresh empty workflows directory in tmp. Tests that need specific
    /// workflows can populate it before constructing the router.
    fn make_hermetic_workflows_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ah-test-workflows-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create hermetic workflows dir");
        dir
    }

    /// Shared router builder that all `test_app*` helpers funnel through. Takes
    /// explicit `workflows_dir`/`approval_mode` so a single test can drop in
    /// custom workflow definitions without diverging from the standard config.
    fn build_test_router(
        approval_mode: ApprovalFeatureMode,
        workflows_dir: std::path::PathBuf,
    ) -> Router {
        router(build_test_state(approval_mode, workflows_dir))
    }

    fn build_test_state(
        approval_mode: ApprovalFeatureMode,
        workflows_dir: std::path::PathBuf,
    ) -> AppState<NullNotifier> {
        let task_dir =
            std::env::temp_dir().join(format!("ah-test-tasks-{}", uuid::Uuid::new_v4().simple()));
        let config_root = make_hermetic_config_root();
        let config = ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
            workflows_dir: workflows_dir.to_string_lossy().into_owned(),
            config_root: config_root.to_string_lossy().into_owned(),
            task_dir: task_dir.to_string_lossy().into_owned(),
            task_state_dir: task_dir
                .with_extension("private-state")
                .to_string_lossy()
                .into_owned(),
            task_adapter: "file".into(),
            vikunja_base_url: None,
            vikunja_token: None,
            vikunja_bot_username: None,
            vikunja_bot_password: None,
            vikunja_project_id: None,
            vikunja_label_prefix: None,
            orchestration_db_path: ":memory:".into(),
            artifact_store_root: "local-agent-data".into(),
            grpc_listen_addr: None,
            github_webhook_secret: None,
            github_polling_enabled: false,
            github_poll_interval_secs: 30,
        };
        AppState::new(config, NullNotifier, None)
    }

    fn test_app() -> Router {
        test_app_with_mode(ApprovalFeatureMode::Readwrite)
    }

    fn test_app_with_mode(approval_mode: ApprovalFeatureMode) -> Router {
        build_test_router(approval_mode, make_hermetic_workflows_dir())
    }

    /// Helper: POST JSON to a path and return the status code.
    async fn post_json(app: &Router, path: &str, body: &str) -> AxumStatus {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    async fn post_json_response(app: &Router, path: &str, body: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn post_json_response_with_worker_session(
        app: &Router,
        path: &str,
        body: &str,
        session_id: &str,
        session_token: &str,
    ) -> axum::response::Response {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("x-agent-hub-worker-session-id", session_id)
            .header("x-agent-hub-worker-session-token", session_token)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn put_json_response_with_worker_session(
        app: &Router,
        path: &str,
        body: &str,
        session_id: &str,
        session_token: &str,
    ) -> axum::response::Response {
        let req = Request::builder()
            .method("PUT")
            .uri(path)
            .header("content-type", "application/json")
            .header("x-agent-hub-worker-session-id", session_id)
            .header("x-agent-hub-worker-session-token", session_token)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn get_response(app: &Router, path: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn get_response_with_worker_session(
        app: &Router,
        path: &str,
        session_id: &str,
        session_token: &str,
    ) -> axum::response::Response {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .header("x-agent-hub-worker-session-id", session_id)
            .header("x-agent-hub-worker-session-token", session_token)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    /// Helper: GET a path and return (status_code, body_string).
    async fn get_json(app: &Router, path: &str) -> (AxumStatus, String) {
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn mobile_approval_queue_renders_readwrite_shell() {
        let app = test_app();

        let (status, body) = get_json(&app, "/approvals/queue").await;

        assert_eq!(status, AxumStatus::OK);
        assert!(body.contains("id=\"queue-state\""));
        assert!(body.contains("id=\"queue-actions\""));
        assert!(body.contains("aria-keyshortcuts=\"a\""));
        assert!(body.contains("aria-keyshortcuts=\"d\""));
        assert!(body.contains("/api/v1/approvals/pending"));
    }

    #[tokio::test]
    async fn mobile_approval_queue_hides_readonly_actions() {
        let app = test_app_with_mode(ApprovalFeatureMode::Readonly);

        let (status, body) = get_json(&app, "/approvals/queue").await;

        assert_eq!(status, AxumStatus::OK);
        assert!(body.contains("id=\"readonly-status\""));
        assert!(!body.contains("id=\"queue-actions\""));
        assert!(!body.contains("aria-keyshortcuts"));
    }

    #[tokio::test]
    async fn mobile_approval_queue_requires_web_auth() {
        let config = ServerConfig {
            auth_mode: AuthMode::Token,
            tokens: vec![Token {
                label: "test".into(),
                secret: protocol::Secret::new("secret"),
            }],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        };
        let app = router(AppState::new(config, NullNotifier, None));

        let request = Request::builder()
            .uri("/approvals/queue")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), AxumStatus::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()["location"], "/auth/login");
    }

    // ---------------------------------------------------------------
    // Status endpoint tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn status_report_opencode_accepted() {
        let app = test_app();
        let status = post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{
                "session_id": "ses_test1",
                "cwd": "/home/nick/myapp",
                "status": "active",
                "editor_type": "opencode"
            }"#,
        )
        .await;
        assert_eq!(
            status,
            AxumStatus::OK,
            "opencode status report should be accepted"
        );
    }

    #[tokio::test]
    async fn root_redirects_to_orchestration() {
        let app = test_app();
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatus::PERMANENT_REDIRECT);
        let location = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok());
        assert_eq!(location, Some("/orchestration"));
    }

    #[tokio::test]
    async fn orchestration_web_routes_remain_available_when_approvals_disabled() {
        let app = test_app_with_mode(ApprovalFeatureMode::Disabled);

        let req = Request::builder()
            .method("GET")
            .uri("/orchestration")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();

        assert_eq!(resp.status(), AxumStatus::OK);

        let req = Request::builder()
            .method("GET")
            .uri("/orchestration/jobs")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatus::OK);

        let req = Request::builder()
            .method("GET")
            .uri("/orchestration/workers")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatus::OK);

        let req = Request::builder()
            .method("GET")
            .uri(format!(
                "/orchestration/jobs/{}/artifacts/stdout_log",
                uuid::Uuid::new_v4()
            ))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), AxumStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn approvals_dashboard_remains_available_when_approval_mode_enabled() {
        let app = test_app_with_mode(ApprovalFeatureMode::Readwrite);

        let req = Request::builder()
            .method("GET")
            .uri("/approvals")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();

        assert_eq!(resp.status(), AxumStatus::OK);
    }

    #[tokio::test]
    async fn status_report_all_editor_types() {
        let app = test_app();
        for editor in ["opencode", "claude", "cursor", "unknown"] {
            let body = format!(
                r#"{{"session_id":"ses_{ed}","cwd":"/tmp","status":"active","editor_type":"{ed}"}}"#,
                ed = editor
            );
            let status = post_json(&app, "/api/v1/hooks/status", &body).await;
            assert_eq!(
                status,
                AxumStatus::OK,
                "editor_type={editor} should be accepted"
            );
        }
    }

    #[tokio::test]
    async fn status_report_without_editor_type() {
        let app = test_app();
        let status = post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_none","cwd":"/tmp","status":"idle"}"#,
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
    }

    #[tokio::test]
    async fn status_report_invalid_editor_type_rejected() {
        let app = test_app();
        let status = post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_bad","cwd":"/tmp","status":"active","editor_type":"vscode"}"#,
        )
        .await;
        assert_eq!(
            status,
            AxumStatus::UNPROCESSABLE_ENTITY,
            "unknown editor_type should be rejected"
        );
    }

    #[tokio::test]
    async fn job_artifact_route_serves_known_key_and_download_header() {
        // This test drives the orchestration end-to-end (create item -> poll
        // job -> finish job -> fetch artifact), so it needs a real workflow in
        // the catalog. Drop a minimal `sample_local_exec` workflow into a
        // hermetic workflows dir before booting the router.
        let workflows_dir = make_hermetic_workflows_dir();
        let workflow_dir = workflows_dir.join("sample_local_exec");
        std::fs::create_dir_all(&workflow_dir).expect("mkdir sample_local_exec");
        std::fs::write(
            workflow_dir.join("workflow.toml"),
            r#"
[workflow]
id = "sample_local_exec"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "command"
executor = "docker"
repo_access = "read_write"
container_image = "local/agent-hub-test:latest"
command = "echo"
args = ["hello"]

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#,
        )
        .expect("write sample_local_exec workflow");
        let app = build_test_router(ApprovalFeatureMode::Readwrite, workflows_dir);

        let create_resp = post_json_response(
            &app,
            "/api/v1/orchestration/work-items",
            r#"{"workflow_id":"__triage__","context":{"task":"artifact-test"}}"#,
        )
        .await;
        assert_eq!(create_resp.status(), AxumStatus::OK);

        let registration = post_json_response(
            &app,
            "/api/v1/orchestration/workers",
            r#"{"id":"worker-artifacts","hostname":"localhost","supported_modes":["docker"],"protocol_capabilities":["canonical_authorization_v1","canonical_authorization_v2","canonical_authorization_v3","canonical_authorization_v4"]}"#,
        )
        .await;
        assert_eq!(registration.status(), AxumStatus::OK);
        assert_eq!(
            registration
                .headers()
                .get("x-agent-hub-server-capabilities")
                .and_then(|value| value.to_str().ok()),
            Some(
                "canonical_authorization_v1, canonical_authorization_v2, canonical_authorization_v3, canonical_authorization_v4"
            )
        );
        let registration: Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(registration.get("server_protocol_capabilities").is_none());
        let session_id = registration["session_id"].as_str().unwrap();
        let session_token = registration["session_token"].as_str().unwrap();

        assert_eq!(
            post_json_response(
                &app,
                "/api/v1/orchestration/poll",
                r#"{"worker_id":"worker-artifacts"}"#,
            )
            .await
            .status(),
            AxumStatus::UNAUTHORIZED
        );
        assert_eq!(
            post_json_response_with_worker_session(
                &app,
                "/api/v1/orchestration/poll",
                r#"{"worker_id":"worker-artifacts"}"#,
                session_id,
                "tampered-token",
            )
            .await
            .status(),
            AxumStatus::UNAUTHORIZED
        );

        let poll_resp = post_json_response_with_worker_session(
            &app,
            "/api/v1/orchestration/poll",
            r#"{"worker_id":"worker-artifacts"}"#,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(poll_resp.status(), AxumStatus::OK);
        let poll_body = axum::body::to_bytes(poll_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let poll_json: Value = serde_json::from_slice(&poll_body).unwrap();
        let job_id = poll_json
            .get("job")
            .and_then(|j| j.get("id"))
            .and_then(Value::as_str)
            .expect("poll should assign a job")
            .to_string();

        let transcript_rel = format!("transcripts/test-{}", uuid::Uuid::new_v4());
        let artifact_root = std::path::PathBuf::from("local-agent-data").join(&transcript_rel);
        std::fs::create_dir_all(&artifact_root).unwrap();
        let stdout_path = artifact_root.join("stdout.log");
        let stderr_path = artifact_root.join("stderr.log");
        std::fs::write(&stdout_path, b"hello stdout\n").unwrap();
        std::fs::write(&stderr_path, b"hello stderr\n").unwrap();

        let running_body = serde_json::json!({
            "worker_id": "worker-artifacts",
            "state": "running",
            "transcript_dir": artifact_root.to_string_lossy(),
            "result": null
        })
        .to_string();

        let running_resp = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &running_body,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(running_resp.status(), AxumStatus::OK);

        let update_body = serde_json::json!({
            "worker_id": "worker-artifacts",
            "state": "succeeded",
            "transcript_dir": transcript_rel,
            "result": {
                "outcome": "succeeded",
                "termination_reason": "exit_code",
                "exit_code": 0,
                "timing": {
                    "started_at": chrono::Utc::now(),
                    "finished_at": chrono::Utc::now(),
                    "duration_ms": 5
                },
                "artifacts": {
                    "stdout_path": format!("{transcript_rel}/stdout.log"),
                    "stderr_path": format!("{transcript_rel}/stderr.log"),
                    "output_path": null,
                    "transcript_dir": transcript_rel,
                    "stdout_bytes": 13,
                    "stderr_bytes": 13,
                    "output_bytes": null,
                    "structured_transcript_artifacts": []
                },
                "output_json": null,
                "repo": null,
                "github_pr": null,
                "error_message": null
            }
        })
        .to_string();

        let update_resp = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &update_body,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(update_resp.status(), AxumStatus::OK);

        let artifact_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{job_id}/artifacts/stdout_log"),
        )
        .await;
        assert_eq!(artifact_resp.status(), AxumStatus::OK);
        assert_eq!(
            artifact_resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );

        let download_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{job_id}/artifacts/stdout_log?download=1"),
        )
        .await;
        assert_eq!(download_resp.status(), AxumStatus::OK);
        let disposition = download_resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(disposition.starts_with("attachment; filename=\""));

        std::fs::remove_file(&stdout_path).unwrap();
        let stale_file_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{job_id}/artifacts/stdout_log"),
        )
        .await;
        assert_eq!(stale_file_resp.status(), AxumStatus::NOT_FOUND);

        let missing_metadata_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{job_id}/artifacts/output_json"),
        )
        .await;
        assert_eq!(missing_metadata_resp.status(), AxumStatus::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&artifact_root);
    }

    #[tokio::test]
    async fn job_artifact_route_rejects_unknown_or_unavailable_artifacts() {
        let app = test_app();

        let missing_job_id = uuid::Uuid::new_v4();

        let bad_key_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{missing_job_id}/artifacts/not_a_key"),
        )
        .await;
        assert_eq!(bad_key_resp.status(), AxumStatus::BAD_REQUEST);

        let dir_key_resp = get_response(
            &app,
            &format!("/orchestration/jobs/{missing_job_id}/artifacts/transcript_dir"),
        )
        .await;
        assert_eq!(dir_key_resp.status(), AxumStatus::BAD_REQUEST);
    }

    // ---------------------------------------------------------------
    // Full session lifecycle: status report → list sessions → verify
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn session_lifecycle_status_reflected_in_api() {
        let app = test_app();

        // 1. Report "active" status from opencode
        let status = post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{
                "session_id": "ses_lifecycle",
                "cwd": "/home/nick/myapp",
                "status": "active",
                "display_name": "Fix auth bug",
                "editor_type": "opencode"
            }"#,
        )
        .await;
        assert_eq!(status, AxumStatus::OK);

        // 2. Verify session appears as Active
        let (status, body) = get_json(&app, "/api/v1/sessions").await;
        assert_eq!(status, AxumStatus::OK);
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_lifecycle")
            .expect("session should exist");
        assert_eq!(sess["status"]["status"], "active");
        assert_eq!(sess["display_name"], "Fix auth bug");
        assert_eq!(sess["editor_type"], "opencode");

        // 3. Report "idle" status
        let status = post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{
                "session_id": "ses_lifecycle",
                "cwd": "/home/nick/myapp",
                "status": "idle",
                "editor_type": "opencode"
            }"#,
        )
        .await;
        assert_eq!(status, AxumStatus::OK);

        // 4. Verify session is now Idle
        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_lifecycle")
            .unwrap();
        assert_eq!(sess["status"]["status"], "idle");

        // 5. Report "active" again (user sent new message)
        post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_lifecycle","cwd":"/home/nick/myapp","status":"active","editor_type":"opencode"}"#,
        )
        .await;

        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_lifecycle")
            .unwrap();
        assert_eq!(sess["status"]["status"], "active");

        // 6. Report "ended"
        post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_lifecycle","cwd":"/home/nick/myapp","status":"ended","editor_type":"opencode"}"#,
        )
        .await;

        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_lifecycle")
            .unwrap();
        assert_eq!(sess["status"]["status"], "ended");
    }

    // ---------------------------------------------------------------
    // Approval overrides session status to "waiting"
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn pending_approval_overrides_session_status_to_waiting() {
        let app = test_app();

        // Register session as active
        post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_approval","cwd":"/tmp/proj","status":"active","editor_type":"opencode"}"#,
        )
        .await;

        // Submit an approval request
        let (status, body) = {
            let req = Request::builder()
                .method("POST")
                .uri("/api/v1/hooks/approval")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "id": "req-1",
                        "session_id": "ses_approval",
                        "session_display_name": "Test Session",
                        "cwd": "/tmp/proj",
                        "tool_name": "Bash",
                        "tool_input": {"command": "rm -rf /"},
                        "provider": "opencode",
                        "request_type": "tool_use",
                        "context": {"workspace_roots": ["/tmp/proj"], "hook_event_name": "permission.ask"}
                    }"#,
                ))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let s = resp.status();
            let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (s, String::from_utf8(b.to_vec()).unwrap())
        };
        assert_eq!(
            status,
            AxumStatus::OK,
            "approval should be registered: {body}"
        );

        // Session should now show as "waiting" due to pending approval
        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_approval")
            .unwrap();
        assert_eq!(
            sess["status"]["status"], "waiting",
            "pending approval should override to waiting"
        );
        let reason = sess["status"]["reason"].as_str().unwrap_or("");
        assert!(
            reason.contains("Pending approval"),
            "reason should mention pending approval, got: {reason}"
        );
    }

    // ---------------------------------------------------------------
    // Zombie Waiting session reset — sessions with no pending
    // question or approval must be reset to Idle so TTL eviction can
    // eventually clean them up.
    // ---------------------------------------------------------------

    /// A Waiting session that has a pending question must NOT be reset.
    #[tokio::test]
    async fn waiting_session_with_pending_question_is_not_reset() {
        let app = test_app();

        // Register a session and set it to Waiting.
        post_json(
            &app,
            "/api/v1/hooks/status",
            r#"{"session_id":"ses_q","cwd":"/tmp/proj","status":"active","editor_type":"opencode"}"#,
        )
        .await;

        // Register a pending question via the hooks endpoint.
        let (status, body) = {
            let req = Request::builder()
                .method("POST")
                .uri("/api/v1/hooks/question")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "id": "req-q1",
                        "session_id": "ses_q",
                        "session_display_name": "Test Session",
                        "cwd": "/tmp/proj",
                        "question_request_id": "oc-req-1",
                        "questions": [{"question":"Continue?","header":"Plan","options":[{"label":"Yes","description":"Go ahead"},{"label":"No","description":"Stop"}]}],
                        "provider": "opencode"
                    }"#,
                ))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let s = resp.status();
            let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (s, String::from_utf8(b.to_vec()).unwrap())
        };
        assert_eq!(
            status,
            AxumStatus::OK,
            "question registration failed: {body}"
        );

        // Session should be Waiting due to the pending question.
        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "ses_q")
            .unwrap();
        assert_eq!(
            sess["status"]["status"], "waiting",
            "session should be Waiting while question is pending"
        );
    }

    #[tokio::test]
    async fn work_item_events_plural_is_canonical_and_singular_is_compatible() {
        let app = test_app();

        for suffix in ["events", "event"] {
            let created = post_json_response(
                &app,
                "/api/v1/orchestration/work-items",
                r#"{"workflow_id":"__triage__","context":{"task":"route-test"}}"#,
            )
            .await;
            assert_eq!(created.status(), AxumStatus::OK);
            let created: Value = serde_json::from_slice(
                &axum::body::to_bytes(created.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let id = created["id"].as_str().expect("work item id");

            let response = post_json_response(
                &app,
                &format!("/api/v1/orchestration/work-items/{id}/{suffix}"),
                r#"{"event":"cancel","job_id":null,"reason":"operator request"}"#,
            )
            .await;
            assert_eq!(response.status(), AxumStatus::OK, "route suffix {suffix}");
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["work_item"]["id"], id);
            assert_eq!(body["cancellation"]["outcome"], "accepted");
            assert!(body["cancellation"]["cancellation_id"].is_string());
        }

        let wrong = post_json_response(
            &app,
            "/api/v1/orchestration/work-items/00000000-0000-0000-0000-000000000000/events/runtime-12",
            r#"{"event":"cancel","job_id":null,"reason":null}"#,
        )
        .await;
        assert_eq!(wrong.status(), AxumStatus::NOT_FOUND);
        let helper_escape =
            post_json_response(&app, "/api/v1/internal/durable-cancellation-e2e", r#"{}"#).await;
        assert_eq!(helper_escape.status(), AxumStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn running_cancellation_is_fenced_until_containment_acknowledgement() {
        let state = build_test_state(
            ApprovalFeatureMode::Readwrite,
            make_hermetic_workflows_dir(),
        );
        let database = state.orchestration.database_path_for_tests().to_path_buf();
        let app = router(state);
        let created = post_json_response(
            &app,
            "/api/v1/orchestration/work-items",
            r#"{"workflow_id":"__triage__","context":{"task":"running-cancel"}}"#,
        )
        .await;
        let created: Value = serde_json::from_slice(
            &axum::body::to_bytes(created.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let work_item_id = created["id"].as_str().unwrap();

        let registration = post_json_response(
            &app,
            "/api/v1/orchestration/workers",
            r#"{"id":"cancel-worker","hostname":"localhost","supported_modes":["docker"],"protocol_capabilities":["canonical_authorization_v1","canonical_authorization_v2","canonical_authorization_v3","canonical_authorization_v4"]}"#,
        )
        .await;
        let registration: Value = serde_json::from_slice(
            &axum::body::to_bytes(registration.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let session_id = registration["session_id"].as_str().unwrap();
        let session_token = registration["session_token"].as_str().unwrap();

        let assigned = post_json_response_with_worker_session(
            &app,
            "/api/v1/orchestration/poll",
            r#"{"worker_id":"cancel-worker"}"#,
            session_id,
            session_token,
        )
        .await;
        let assigned: Value = serde_json::from_slice(
            &axum::body::to_bytes(assigned.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let job_id = assigned["job"]["id"].as_str().unwrap();
        let running = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            r#"{"worker_id":"cancel-worker","state":"running","transcript_dir":null,"result":null}"#,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(running.status(), AxumStatus::OK);

        let mut blocker = rusqlite::Connection::open(&database).unwrap();
        blocker
            .busy_timeout(std::time::Duration::from_secs(15))
            .unwrap();
        let blocker_tx = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        blocker_tx
            .execute(
                "UPDATE workers SET state = state WHERE id = ?1",
                ["cancel-worker"],
            )
            .unwrap();

        let cancel_app = app.clone();
        let cancel_path = format!("/api/v1/orchestration/work-items/{work_item_id}/events");
        let cancel_task = tokio::spawn(async move {
            post_json_response(
                &cancel_app,
                &cancel_path,
                r#"{"event":"cancel","job_id":null,"reason":"operator request"}"#,
            )
            .await
        });
        let heartbeat_app = app.clone();
        let heartbeat_session_id = session_id.to_string();
        let heartbeat_session_token = session_token.to_string();
        let heartbeat_task = tokio::spawn(async move {
            post_json_response_with_worker_session(
                &heartbeat_app,
                "/api/v1/orchestration/workers/cancel-worker/heartbeat",
                r#"{"state":"busy"}"#,
                &heartbeat_session_id,
                &heartbeat_session_token,
            )
            .await
        });
        let poll_app = app.clone();
        let poll_session_id = session_id.to_string();
        let poll_session_token = session_token.to_string();
        let poll_task = tokio::spawn(async move {
            post_json_response_with_worker_session(
                &poll_app,
                "/api/v1/orchestration/poll",
                r#"{"worker_id":"cancel-worker"}"#,
                &poll_session_id,
                &poll_session_token,
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(!cancel_task.is_finished());
        blocker_tx.commit().unwrap();

        let cancelled = cancel_task.await.unwrap();
        let heartbeat = heartbeat_task.await.unwrap();
        let poll = poll_task.await.unwrap();
        assert_ne!(heartbeat.status(), AxumStatus::INTERNAL_SERVER_ERROR);
        assert_ne!(poll.status(), AxumStatus::INTERNAL_SERVER_ERROR);
        assert_eq!(cancelled.status(), AxumStatus::OK);
        let cancelled: Value = serde_json::from_slice(
            &axum::body::to_bytes(cancelled.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cancelled["work_item"]["state"], "cancelling");
        assert_eq!(cancelled["cancellation"]["running_jobs"], 1);
        let cancellation_id = cancelled["cancellation"]["cancellation_id"]
            .as_str()
            .unwrap();

        let replacement = post_json_response_with_worker_session(
            &app,
            "/api/v1/orchestration/workers",
            r#"{"id":"cancel-worker","hostname":"localhost-restarted","supported_modes":["docker"],"protocol_capabilities":["canonical_authorization_v1","canonical_authorization_v2","canonical_authorization_v3","canonical_authorization_v4"]}"#,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(replacement.status(), AxumStatus::OK);
        let replacement: Value = serde_json::from_slice(
            &axum::body::to_bytes(replacement.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let recovery_session_id = replacement["session_id"].as_str().unwrap();
        let recovery_session_token = replacement["session_token"].as_str().unwrap();
        assert_ne!(recovery_session_id, session_id);

        let stale_poll = post_json_response_with_worker_session(
            &app,
            "/api/v1/orchestration/poll",
            r#"{"worker_id":"cancel-worker"}"#,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(stale_poll.status(), AxumStatus::UNAUTHORIZED);

        let fallback = post_json_response_with_worker_session(
            &app,
            "/api/v1/orchestration/poll",
            r#"{"worker_id":"cancel-worker"}"#,
            recovery_session_id,
            recovery_session_token,
        )
        .await;
        let fallback: Value = serde_json::from_slice(
            &axum::body::to_bytes(fallback.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(fallback["job"].is_null());
        assert_eq!(fallback["cancellation"]["cancellation_id"], cancellation_id);
        assert_eq!(fallback["cancellation"]["generation"], 2);
        assert_eq!(
            fallback["cancellation"]["worker_session_id"],
            recovery_session_id
        );

        let now = chrono::Utc::now().to_rfc3339();
        let late_success = serde_json::json!({
            "worker_id": "cancel-worker",
            "state": "succeeded",
            "transcript_dir": "transcripts/late-success",
            "result": {
                "outcome": "succeeded",
                "termination_reason": "exit_code",
                "exit_code": 0,
                "timing": {"started_at": now, "finished_at": now, "duration_ms": 0},
                "artifacts": {
                    "stdout_path": "transcripts/late-success/stdout.log",
                    "stderr_path": "transcripts/late-success/stderr.log",
                    "output_path": null,
                    "transcript_dir": "transcripts/late-success",
                    "structured_transcript_artifacts": []
                },
                "output_json": {"next_event":"job_succeeded"},
                "repo": null,
                "github_pr": null,
                "error_message": null
            }
        })
        .to_string();
        let late_success = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &late_success,
            session_id,
            session_token,
        )
        .await;
        assert_eq!(late_success.status(), AxumStatus::UNAUTHORIZED);

        let late_success = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &serde_json::json!({
                "worker_id": "cancel-worker",
                "state": "succeeded",
                "transcript_dir": "transcripts/late-success",
                "result": {
                    "outcome": "succeeded",
                    "termination_reason": "exit_code",
                    "exit_code": 0,
                    "timing": {"started_at": now, "finished_at": now, "duration_ms": 0},
                    "artifacts": {
                        "stdout_path": "transcripts/late-success/stdout.log",
                        "stderr_path": "transcripts/late-success/stderr.log",
                        "output_path": null,
                        "transcript_dir": "transcripts/late-success",
                        "structured_transcript_artifacts": []
                    },
                    "output_json": {"next_event":"job_succeeded"},
                    "repo": null,
                    "github_pr": null,
                    "error_message": null
                }
            })
            .to_string(),
            recovery_session_id,
            recovery_session_token,
        )
        .await;
        assert_eq!(late_success.status(), AxumStatus::BAD_REQUEST);

        let failed_containment = serde_json::json!({
            "worker_id": "cancel-worker",
            "state": "cancelled",
            "transcript_dir": "transcripts/cancelled",
            "result": {
                "outcome": "cancelled",
                "termination_reason": "cancelled",
                "exit_code": null,
                "timing": {"started_at": now, "finished_at": now, "duration_ms": 0},
                "artifacts": {
                    "stdout_path": "transcripts/cancelled/stdout.log",
                    "stderr_path": "transcripts/cancelled/stderr.log",
                    "output_path": null,
                    "transcript_dir": "transcripts/cancelled",
                    "structured_transcript_artifacts": []
                },
                "output_json": null,
                "repo": null,
                "github_pr": null,
                "error_message": "synthetic rm failure",
                "cancellation_acknowledgement": {
                    "command": fallback["cancellation"].clone(),
                    "containment": {
                        "checked_at": now,
                        "process": "verified_absent",
                        "container": {"failed":{"error":"synthetic rm failure"}}
                    }
                }
            }
        })
        .to_string();
        let failed_containment = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &failed_containment,
            recovery_session_id,
            recovery_session_token,
        )
        .await;
        assert_eq!(failed_containment.status(), AxumStatus::BAD_REQUEST);

        let containment_status = post_json_response(
            &app,
            &format!("/api/v1/orchestration/work-items/{work_item_id}/events"),
            r#"{"event":"cancel","job_id":null,"reason":"containment retry"}"#,
        )
        .await;
        let containment_status: Value = serde_json::from_slice(
            &axum::body::to_bytes(containment_status.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            containment_status["work_item"]["context"]["cancellation"]["state"],
            "containment_failed"
        );
        assert_eq!(
            containment_status["work_item"]["context"]["cancellation"]["cleanup_error"],
            "synthetic rm failure"
        );

        let acknowledgement = serde_json::json!({
            "worker_id": "cancel-worker",
            "state": "cancelled",
            "transcript_dir": "transcripts/cancelled",
            "result": {
                "outcome": "cancelled",
                "termination_reason": "cancelled",
                "exit_code": null,
                "timing": {"started_at": now, "finished_at": now, "duration_ms": 0},
                "artifacts": {
                    "stdout_path": "transcripts/cancelled/stdout.log",
                    "stderr_path": "transcripts/cancelled/stderr.log",
                    "output_path": null,
                    "transcript_dir": "transcripts/cancelled",
                    "structured_transcript_artifacts": []
                },
                "output_json": null,
                "repo": null,
                "github_pr": null,
                "error_message": null,
                "cancellation_acknowledgement": {
                    "command": fallback["cancellation"].clone(),
                    "containment": {
                        "checked_at": now,
                        "process": "verified_absent",
                        "container": "verified_absent"
                    }
                }
            }
        })
        .to_string();
        let acknowledged = put_json_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            &acknowledgement,
            recovery_session_id,
            recovery_session_token,
        )
        .await;
        assert_eq!(acknowledged.status(), AxumStatus::OK);
        let committed = get_response_with_worker_session(
            &app,
            &format!("/api/v1/orchestration/jobs/{job_id}"),
            recovery_session_id,
            recovery_session_token,
        )
        .await;
        assert_eq!(committed.status(), AxumStatus::OK);
        let committed: Value = serde_json::from_slice(
            &axum::body::to_bytes(committed.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(committed["state"], "cancelled");
        assert_eq!(
            committed["result"]["cancellation_acknowledgement"]["command"],
            fallback["cancellation"]
        );

        let duplicate = post_json_response(
            &app,
            &format!("/api/v1/orchestration/work-items/{work_item_id}/events"),
            r#"{"event":"cancel","job_id":null,"reason":"duplicate"}"#,
        )
        .await;
        assert_eq!(duplicate.status(), AxumStatus::OK);
        let duplicate: Value = serde_json::from_slice(
            &axum::body::to_bytes(duplicate.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(duplicate["work_item"]["state"], "cancelled");
        assert_eq!(
            duplicate["cancellation"]["cancellation_id"],
            cancellation_id
        );
        assert_eq!(duplicate["cancellation"]["outcome"], "already_requested");

        let connection = rusqlite::Connection::open(database).unwrap();
        let command: (String, String, String) = connection
            .query_row(
                "SELECT cancellation_id, delivery_state, acknowledgement_json
                 FROM job_cancellation_commands WHERE job_id = ?1",
                [job_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(command.0, cancellation_id);
        assert_eq!(command.1, "acknowledged");
        assert!(!command.2.is_empty());
        let acknowledgement: Value = serde_json::from_str(&command.2).unwrap();
        let result_json: String = connection
            .query_row(
                "SELECT result_json FROM jobs WHERE id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .unwrap();
        let result: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(result["cancellation_acknowledgement"], acknowledgement);
        let work_item_cancellation: (String, String) = connection
            .query_row(
                "SELECT cancellation_id, state FROM work_item_cancellations
                 WHERE work_item_id = ?1",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(work_item_cancellation.0, cancellation_id);
        assert_eq!(work_item_cancellation.1, "completed");
        let active: u32 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM job_cancellation_commands
                    WHERE delivery_state != 'acknowledged') +
                    (SELECT COUNT(*) FROM work_item_cancellations
                    WHERE state != 'completed')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }

    /// A Waiting session whose question has been resolved must be eligible for
    /// zombie reset (first_pending_for_session returns None after resolve).
    #[tokio::test]
    async fn waiting_session_becomes_zombie_after_question_resolved() {
        use protocol::QuestionInfo;
        use questions::{QuestionRegistry, QuestionStatus, RegisterQuestion};
        use sessions::SessionStatus;

        let reg = SessionRegistry::new(9999);
        let questions = QuestionRegistry::new();

        // Register session in Waiting state.
        reg.get_or_register(&SessionId::new("ses_zombie"), "/tmp/z", None)
            .await;
        reg.set_status(
            &SessionId::new("ses_zombie"),
            SessionStatus::Waiting,
            Some("pending question".to_string()),
            None,
        )
        .await;

        // Register a question for this session.
        let q = questions
            .register(RegisterQuestion {
                request_id: "req-zombie-1".to_string(),
                session_id: SessionId::new("ses_zombie"),
                session_display_name: "Zombie Test".to_string(),
                project: "z".to_string(),
                question_request_id: "oc-1".to_string(),
                questions: vec![QuestionInfo {
                    question: "Proceed?".to_string(),
                    header: "Plan".to_string(),
                    options: vec![],
                    multiple: None,
                    custom: None,
                }],
                provider: "opencode".to_string(),
            })
            .await;

        // While question is pending, first_pending_for_session returns Some.
        assert!(
            questions
                .first_pending_for_session(&SessionId::new("ses_zombie"))
                .await
                .is_some(),
            "should have a pending question"
        );

        // Simulate gateway cancel (e.g. SIGTERM path).
        questions.resolve(q.id, QuestionStatus::Cancelled).await;

        // Now no pending question remains — zombie condition.
        assert!(
            questions
                .first_pending_for_session(&SessionId::new("ses_zombie"))
                .await
                .is_none(),
            "no pending question after cancel"
        );

        // The zombie reset logic in main.rs would call set_status(Idle) here.
        reg.set_status(
            &SessionId::new("ses_zombie"),
            SessionStatus::Idle,
            None,
            None,
        )
        .await;

        let sessions = reg.list().await;
        let sess = sessions
            .iter()
            .find(|s| s.session_id == SessionId::new("ses_zombie"))
            .unwrap();
        assert_eq!(
            sess.stored_status,
            SessionStatus::Idle,
            "zombie session should be reset to Idle"
        );
        assert!(
            sess.waiting_reason.is_none(),
            "waiting_reason should be cleared after reset"
        );
    }
}

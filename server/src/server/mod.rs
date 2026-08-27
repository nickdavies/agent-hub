pub mod approvals;
pub mod auth;
pub mod config;
pub mod hooks;
pub mod notifier;
pub mod oauth;
pub mod presence;
pub mod pushover;
pub mod questions;
pub mod sessions;
pub mod storage;
pub mod web;
pub mod webhook;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::rejection::JsonRejection;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tower_sessions::{MemoryStore, SessionManagerLayer};
use uuid::Uuid;

use crate::error::AppError;
use crate::mcp;
use approvals::{ApprovalRegistry, ApprovalStatus};
use config::{
    ApprovalFeatureMode, AuthMode, NotifyConfig, NotifyConfigUpdate, ServerConfig,
    SharedNotifyConfig,
};
use hooks::PendingNotifications;
use notifier::Notifier;
use oauth::OAuthManager;
use presence::{Presence, PresenceUpdate};
use questions::QuestionRegistry;
use sessions::{EffectiveSessionStatus, SessionConfigUpdate, SessionRegistry, SessionStatus};

// Import protocol types used directly in this module's handlers.
use protocol::{
    ApprovalDecision, ApprovalModeResponse, ApprovalResolveRequest, ApprovalWaitResponse,
    ConfigResponse, QuestionDecision, QuestionResolveRequest, QuestionWaitResponse, SessionId,
};

pub struct AppState<N: Notifier> {
    pub config: Arc<ServerConfig>,
    pub presence: Arc<Presence>,
    pub sessions: Arc<SessionRegistry>,
    pub notifier: Arc<N>,
    pub notify_config: SharedNotifyConfig,
    pub pending: Arc<PendingNotifications>,
    pub approvals: Arc<ApprovalRegistry>,
    pub questions: Arc<QuestionRegistry>,
    pub oauth: Arc<Option<OAuthManager>>,
}

impl<N: Notifier> Clone for AppState<N> {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            presence: Arc::clone(&self.presence),
            sessions: Arc::clone(&self.sessions),
            notifier: Arc::clone(&self.notifier),
            notify_config: Arc::clone(&self.notify_config),
            pending: Arc::clone(&self.pending),
            approvals: Arc::clone(&self.approvals),
            questions: Arc::clone(&self.questions),
            oauth: Arc::clone(&self.oauth),
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

    // Redirect root to the dashboard
    app = app.route(
        "/",
        get(|| async { axum::response::Redirect::permanent("/approvals") }),
    );

    // Mount web UI and OAuth routes when approval mode is not disabled
    if state.config.approval_mode != ApprovalFeatureMode::Disabled {
        // Web UI routes
        let mut web_routes = Router::new()
            .route("/approvals", get(web::dashboard::<N>))
            .route("/approvals/queue", get(web::approval_queue::<N>))
            .route("/approvals/{id}", get(web::approval_detail::<N>));

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
    }

    // Session layer for OAuth (in-memory store, sessions lost on restart)
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store);

    app.layer(session_layer)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}

/// Resolve the effective status for a session by combining its stored status
/// with server-side knowledge (pending approvals, pending questions).
pub(crate) fn resolve_effective_status(
    stored: SessionStatus,
    waiting_reason: Option<&str>,
    pending_approval: Option<&approvals::Approval>,
    pending_question: Option<&questions::PendingQuestion>,
) -> EffectiveSessionStatus {
    if stored == SessionStatus::Ended {
        return EffectiveSessionStatus::Ended;
    }
    // Pending approvals always win — they're actionable in the UI
    if let Some(approval) = pending_approval {
        let input_str = approval.tool_input.to_string();
        let truncated = if input_str.len() > 60 {
            let end = input_str
                .char_indices()
                .nth(60)
                .map_or(input_str.len(), |(i, _)| i);
            format!("{}...", &input_str[..end])
        } else {
            input_str
        };
        let reason = format!("Pending approval: {} — {}", approval.tool, truncated);
        return EffectiveSessionStatus::Waiting {
            reason: Some(reason),
        };
    }
    // Pending questions are also waiting
    if let Some(pq) = pending_question {
        let header = pq
            .questions
            .first()
            .map(|q| q.header.as_str())
            .unwrap_or("question");
        let reason = format!("Plan question: {header}");
        return EffectiveSessionStatus::Waiting {
            reason: Some(reason),
        };
    }
    // Client-reported waiting
    if stored == SessionStatus::Waiting {
        return EffectiveSessionStatus::Waiting {
            reason: waiting_reason.map(|s| s.to_string()),
        };
    }
    match stored {
        SessionStatus::Active => EffectiveSessionStatus::Active,
        SessionStatus::Idle => EffectiveSessionStatus::Idle,
        // Ended and Waiting are handled by the early returns above.
        // Listing them explicitly so adding a new SessionStatus variant
        // produces a compile-time error instead of a runtime panic.
        SessionStatus::Ended | SessionStatus::Waiting => {
            debug_assert!(false, "Ended/Waiting should have been handled above");
            EffectiveSessionStatus::Active
        }
    }
}

/// Build a list of SessionViews with effective status resolved.
async fn build_session_views<N: Notifier>(state: &AppState<N>) -> Vec<sessions::SessionView> {
    let raw = state.sessions.list().await;
    let mut views = Vec::with_capacity(raw.len());
    for s in raw {
        let pending = state
            .approvals
            .first_pending_for_session(&s.session_id)
            .await;
        let pending_q = state
            .questions
            .first_pending_for_session(&s.session_id)
            .await;
        let status = resolve_effective_status(
            s.stored_status,
            s.waiting_reason.as_deref(),
            pending.as_ref(),
            pending_q.as_ref(),
        );
        views.push(sessions::SessionView {
            session_id: s.session_id,
            project: s.project,
            config: s.config,
            editor_type: s.editor_type,
            status,
            display_name: s.display_name,
        });
    }
    views
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
    body: Result<Json<ApprovalResolveRequest>, JsonRejection>,
) -> Result<Json<approvals::Approval>, AppError> {
    let Json(req) = body.map_err(|error| AppError::BadRequest(error.body_text()))?;
    if req.approve_for.is_some() && req.decision != ApprovalDecision::Approve {
        return Err(AppError::BadRequest(
            "approve_for is valid only with an approve decision".to_string(),
        ));
    }

    let approve_for = req.approve_for;
    let new_status = match req.decision {
        ApprovalDecision::Approve => ApprovalStatus::Approved {
            message: req.message,
        },
        ApprovalDecision::Deny => ApprovalStatus::Denied {
            reason: req.message.unwrap_or_default(),
        },
        ApprovalDecision::Cancel => ApprovalStatus::Cancelled,
    };

    if let Some(duration) = approve_for {
        state
            .approvals
            .resolve_with_grant(id, new_status, duration)
            .await
            .map(Json)
            .map_err(|error| match error {
                approvals::TemporaryGrantError::ApprovalNotFound => {
                    AppError::ApprovalNotFound(id.to_string())
                }
                _ => AppError::BadRequest(error.to_string()),
            })
    } else {
        state
            .approvals
            .resolve(id, new_status)
            .await
            .ok_or_else(|| AppError::ApprovalNotFound(id.to_string()))
            .map(Json)
    }
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
        let presence = Presence::new(server_config.presence_ttl_secs);
        let sessions = SessionRegistry::new(server_config.session_ttl_secs)
            .with_default_approval_mode(server_config.default_approval_mode);
        let notify_config = NotifyConfig::with_delay(server_config.notification_delay_secs);

        Self {
            config: Arc::new(server_config),
            presence: Arc::new(presence),
            sessions: Arc::new(sessions),
            notifier: Arc::new(notifier),
            notify_config: Arc::new(RwLock::new(notify_config)),
            pending: Arc::new(PendingNotifications::new()),
            approvals: Arc::new(ApprovalRegistry::new()),
            questions: Arc::new(QuestionRegistry::new()),
            oauth: Arc::new(oauth),
        }
    }

    /// Capture current state for persistence.
    pub async fn snapshot(&self) -> storage::PersistedState {
        self.snapshot_at(Utc::now(), Instant::now()).await
    }

    async fn snapshot_at(
        &self,
        wall_now: DateTime<Utc>,
        monotonic_now: Instant,
    ) -> storage::PersistedState {
        let (pending_approvals, timed_approval_grants) =
            self.approvals.snapshot_at(wall_now, monotonic_now).await;
        storage::PersistedState {
            sessions: self.sessions.snapshot().await,
            notify_config: Some(self.notify_config.read().await.clone()),
            presence: Some(self.presence.raw_state().await),
            pending_approvals,
            timed_approval_grants,
        }
    }

    pub async fn restore_from_storage(
        &self,
        storage: &impl storage::Storage,
    ) -> anyhow::Result<()> {
        self.restore_from_storage_at(storage, Utc::now(), Instant::now())
            .await
    }

    async fn restore_from_storage_at(
        &self,
        storage: &impl storage::Storage,
        wall_now: DateTime<Utc>,
        monotonic_now: Instant,
    ) -> anyhow::Result<()> {
        let Some(mut state) = storage
            .load()
            .await
            .context("failed to load persisted state")?
        else {
            return Ok(());
        };
        tracing::info!(sessions = state.sessions.len(), "restoring persisted state");

        let timed_approval_grants = std::mem::take(&mut state.timed_approval_grants);
        if !timed_approval_grants.is_empty() {
            storage.save(&state).await?;
            state.timed_approval_grants = timed_approval_grants;
        }

        self.restore_at(state, wall_now, monotonic_now).await;
        Ok(())
    }

    async fn restore_at(
        &self,
        state: storage::PersistedState,
        wall_now: DateTime<Utc>,
        monotonic_now: Instant,
    ) {
        let storage::PersistedState {
            sessions,
            notify_config,
            presence,
            pending_approvals,
            timed_approval_grants,
        } = state;
        self.sessions.restore(sessions).await;
        if let Some(cfg) = notify_config {
            *self.notify_config.write().await = cfg;
        }
        if let Some(presence) = presence {
            self.presence.set(presence).await;
        }
        if !pending_approvals.is_empty() {
            self.approvals.restore(pending_approvals).await;
        }
        self.approvals
            .restore_grants_at(timed_approval_grants, wall_now, monotonic_now)
            .await;
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
    use axum::http::{Request, StatusCode as AxumStatus};
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use config::{ApprovalFeatureMode, AuthMode, ServerConfig, Token};
    use notifier::{Notifier, NotifyError, NullNotifier};
    use protocol::{
        ApprovalContext, ApprovalGrantDuration, ApprovalRequest, HookEventName, RequestType, Tool,
    };
    use sessions::SessionApprovalMode;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tower::ServiceExt; // for oneshot

    struct MemoryStorage {
        state: Mutex<Option<Vec<u8>>>,
        fail_saves: AtomicBool,
    }

    impl MemoryStorage {
        fn new(state: storage::PersistedState) -> Self {
            Self {
                state: Mutex::new(Some(
                    serde_json::to_vec(&state).expect("persisted state should serialize"),
                )),
                fail_saves: AtomicBool::new(false),
            }
        }

        fn fail_saves(&self) {
            self.fail_saves.store(true, Ordering::SeqCst);
        }

        fn persisted_state(&self) -> storage::PersistedState {
            let bytes = self
                .state
                .lock()
                .expect("memory storage lock should not be poisoned")
                .clone()
                .expect("memory storage should contain state");
            serde_json::from_slice(&bytes).expect("persisted state should deserialize")
        }
    }

    impl storage::Storage for MemoryStorage {
        async fn load(&self) -> anyhow::Result<Option<storage::PersistedState>> {
            self.state
                .lock()
                .expect("memory storage lock should not be poisoned")
                .as_deref()
                .map(serde_json::from_slice)
                .transpose()
                .map_err(Into::into)
        }

        async fn save(&self, state: &storage::PersistedState) -> anyhow::Result<()> {
            if self.fail_saves.load(Ordering::SeqCst) {
                anyhow::bail!("injected save failure");
            }
            *self
                .state
                .lock()
                .expect("memory storage lock should not be poisoned") =
                Some(serde_json::to_vec(state)?);
            Ok(())
        }
    }

    struct CountingNotifier {
        sends: Arc<AtomicUsize>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CapturedNotification {
        title: String,
        message: String,
        url: Option<String>,
    }

    struct CapturingNotifier {
        sends: Arc<std::sync::Mutex<Vec<CapturedNotification>>>,
        sent: Arc<tokio::sync::Notify>,
    }

    impl Notifier for CountingNotifier {
        fn name(&self) -> &'static str {
            "counting"
        }

        async fn send(
            &self,
            _title: &str,
            _message: &str,
            _url: Option<&str>,
        ) -> Result<(), NotifyError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Notifier for CapturingNotifier {
        fn name(&self) -> &'static str {
            "capturing"
        }

        async fn send(
            &self,
            title: &str,
            message: &str,
            url: Option<&str>,
        ) -> Result<(), NotifyError> {
            self.sends
                .lock()
                .expect("captured notifications lock should not be poisoned")
                .push(CapturedNotification {
                    title: title.to_string(),
                    message: message.to_string(),
                    url: url.map(str::to_string),
                });
            self.sent.notify_one();
            Ok(())
        }
    }

    /// Build a test router with auth disabled (no Bearer tokens needed).
    fn test_app_with_mode(approval_mode: ApprovalFeatureMode) -> Router {
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
        };
        let state = AppState::new(config, NullNotifier, None);
        router(state)
    }

    fn test_app() -> Router {
        test_app_with_mode(ApprovalFeatureMode::Readwrite)
    }

    fn test_state_with_mode(approval_mode: ApprovalFeatureMode) -> AppState<NullNotifier> {
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
        };
        AppState::new(config, NullNotifier, None)
    }

    async fn register_web_approval(
        state: &AppState<NullNotifier>,
        request_id: &str,
        tool: Tool,
        tool_input: serde_json::Value,
    ) -> approvals::Approval {
        state
            .approvals
            .register(approvals::RegisterApproval {
                request_id: request_id.to_string(),
                session_id: SessionId::new(format!("session-{request_id}")),
                session_display_name: "test session".to_string(),
                project: "/workspace".to_string(),
                tool,
                tool_input,
                provider: "opencode".to_string(),
                request_type: RequestType::ToolUse,
                context: ApprovalContext {
                    workspace_roots: vec!["/workspace".to_string()],
                    hook_event_name: HookEventName::Other("test".to_string()),
                    extra: None,
                },
            })
            .await
    }

    async fn register_bash_approval_at(
        state: &AppState<NullNotifier>,
        request_id: &str,
        session_id: &str,
        command: &str,
        wall_now: chrono::DateTime<Utc>,
        monotonic_now: tokio::time::Instant,
    ) -> approvals::Approval {
        state
            .approvals
            .register_at(
                approvals::RegisterApproval {
                    request_id: request_id.to_string(),
                    session_id: SessionId::new(session_id),
                    session_display_name: "test session".to_string(),
                    project: "/workspace".to_string(),
                    tool: Tool::Bash,
                    tool_input: serde_json::json!({"command": command}),
                    provider: "opencode".to_string(),
                    request_type: RequestType::ToolUse,
                    context: ApprovalContext {
                        workspace_roots: vec!["/workspace".to_string()],
                        hook_event_name: HookEventName::Other("test".to_string()),
                        extra: None,
                    },
                },
                wall_now,
                monotonic_now,
            )
            .await
    }

    fn fixed_wall_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn restored_grant_is_consumed_before_activation_and_cannot_replay_after_crash() {
        let source = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let issued_at = fixed_wall_now();
        let source_monotonic = tokio::time::Instant::now();
        let long_grant = register_bash_approval_at(
            &source,
            "long-grant",
            "session-1",
            "cargo test",
            issued_at,
            source_monotonic,
        )
        .await;
        let replacement = register_bash_approval_at(
            &source,
            "short-replacement",
            "session-1",
            "cargo test",
            issued_at,
            source_monotonic,
        )
        .await;
        source
            .approvals
            .resolve_with_grant_at(
                long_grant.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::TwentyFourHours,
                source_monotonic,
            )
            .await
            .unwrap();
        let storage = MemoryStorage::new(source.snapshot_at(issued_at, source_monotonic).await);

        let first_start = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let first_start_monotonic = tokio::time::Instant::now();
        first_start
            .restore_from_storage_at(&storage, issued_at, first_start_monotonic)
            .await
            .expect("startup should consume and restore the persisted state");
        assert!(
            storage.persisted_state().timed_approval_grants.is_empty(),
            "persisted authority must be consumed before startup succeeds"
        );

        first_start
            .approvals
            .resolve_with_grant_at(
                replacement.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                first_start_monotonic,
            )
            .await
            .expect("the restored pending approval should create a shorter replacement");
        let before_short_expiry = register_bash_approval_at(
            &first_start,
            "first-runtime-match",
            "session-1",
            "cargo test",
            issued_at,
            first_start_monotonic + Duration::from_secs(29 * 60),
        )
        .await;
        assert_eq!(
            before_short_expiry.status,
            ApprovalStatus::Approved { message: None }
        );

        // Simulate a crash by starting from the storage copy without saving the
        // first runtime's shorter replacement.
        let crash_restart = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let crash_restart_monotonic = tokio::time::Instant::now();
        crash_restart
            .restore_from_storage_at(
                &storage,
                issued_at + ChronoDuration::hours(1),
                crash_restart_monotonic,
            )
            .await
            .expect("consumed state should remain restartable");
        let after_crash = register_bash_approval_at(
            &crash_restart,
            "after-crash",
            "session-1",
            "cargo test",
            issued_at + ChronoDuration::hours(1),
            crash_restart_monotonic,
        )
        .await;

        assert_eq!(
            after_crash.status,
            ApprovalStatus::Pending,
            "the original 24-hour grant must not replay after a crash"
        );
    }

    #[tokio::test]
    async fn persisted_grant_consumption_failure_fails_closed_before_restore() {
        let source = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let issued_at = fixed_wall_now();
        let source_monotonic = tokio::time::Instant::now();
        register_bash_approval_at(
            &source,
            "persisted-pending",
            "session-2",
            "cargo check",
            issued_at,
            source_monotonic,
        )
        .await;
        let grant = register_bash_approval_at(
            &source,
            "grant-source",
            "session-1",
            "cargo test",
            issued_at,
            source_monotonic,
        )
        .await;
        source
            .approvals
            .resolve_with_grant_at(
                grant.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::TwentyFourHours,
                source_monotonic,
            )
            .await
            .unwrap();
        let storage = MemoryStorage::new(source.snapshot_at(issued_at, source_monotonic).await);
        storage.fail_saves();
        let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let startup_monotonic = tokio::time::Instant::now();

        let error = restored
            .restore_from_storage_at(&storage, issued_at, startup_monotonic)
            .await
            .expect_err("startup must fail when persisted authority cannot be consumed");

        assert!(error.to_string().contains("injected save failure"));
        assert_eq!(restored.approvals.grant_count().await, 0);
        assert!(restored.approvals.list_pending().await.is_empty());
        let matching = register_bash_approval_at(
            &restored,
            "after-failed-restore",
            "session-1",
            "cargo test",
            issued_at,
            startup_monotonic,
        )
        .await;
        assert_eq!(matching.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn clean_snapshot_restores_unexpired_grant_with_remaining_monotonic_lifetime() {
        let source = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let issued_at = fixed_wall_now();
        let monotonic_issued_at = tokio::time::Instant::now();
        let granted = register_bash_approval_at(
            &source,
            "grant-source",
            "session-1",
            "cargo test",
            issued_at,
            monotonic_issued_at,
        )
        .await;
        source
            .approvals
            .resolve_with_grant_at(
                granted.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_issued_at,
            )
            .await
            .unwrap();

        let shutdown_wall = issued_at + ChronoDuration::minutes(10);
        let shutdown_monotonic = monotonic_issued_at + Duration::from_secs(10 * 60);
        let persisted = source.snapshot_at(shutdown_wall, shutdown_monotonic).await;
        assert_eq!(persisted.timed_approval_grants.len(), 1);
        let persisted_grant = &persisted.timed_approval_grants[0];
        assert_eq!(persisted_grant.session_id, SessionId::new("session-1"));
        assert_eq!(persisted_grant.command, "cargo test");
        assert_eq!(persisted_grant.issued_at, shutdown_wall);
        assert_eq!(
            persisted_grant.expires_at,
            issued_at + ChronoDuration::minutes(30)
        );

        let startup_wall = issued_at + ChronoDuration::minutes(15);
        let startup_monotonic = tokio::time::Instant::now();
        let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        restored
            .restore_at(persisted, startup_wall, startup_monotonic)
            .await;
        let restored_expiry = startup_monotonic + Duration::from_secs(15 * 60);

        let before_expiry = register_bash_approval_at(
            &restored,
            "before-expiry",
            "session-1",
            "cargo test",
            startup_wall + ChronoDuration::days(365),
            restored_expiry - Duration::from_nanos(1),
        )
        .await;
        let at_expiry = register_bash_approval_at(
            &restored,
            "at-expiry",
            "session-1",
            "cargo test",
            startup_wall - ChronoDuration::days(365),
            restored_expiry,
        )
        .await;
        let after_expiry = register_bash_approval_at(
            &restored,
            "after-expiry",
            "session-1",
            "cargo test",
            startup_wall,
            restored_expiry + Duration::from_nanos(1),
        )
        .await;

        assert_eq!(
            before_expiry.status,
            ApprovalStatus::Approved { message: None }
        );
        assert_eq!(at_expiry.status, ApprovalStatus::Pending);
        assert_eq!(after_expiry.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn clean_snapshot_purges_expired_timed_grants_and_retains_valid_grants() {
        let state = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let issued_at = fixed_wall_now();
        let monotonic_issued_at = tokio::time::Instant::now();
        let expired = register_bash_approval_at(
            &state,
            "expired-source",
            "expired-session",
            "cargo test --expired",
            issued_at,
            monotonic_issued_at,
        )
        .await;
        let valid = register_bash_approval_at(
            &state,
            "valid-source",
            "valid-session",
            "cargo test --valid",
            issued_at,
            monotonic_issued_at,
        )
        .await;
        state
            .approvals
            .resolve_with_grant_at(
                expired.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_issued_at,
            )
            .await
            .unwrap();
        state
            .approvals
            .resolve_with_grant_at(
                valid.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::OneHour,
                monotonic_issued_at,
            )
            .await
            .unwrap();

        let snapshot_wall = issued_at + ChronoDuration::minutes(30);
        let snapshot_monotonic = monotonic_issued_at + Duration::from_secs(30 * 60);
        let snapshot = state.snapshot_at(snapshot_wall, snapshot_monotonic).await;

        assert_eq!(snapshot.timed_approval_grants.len(), 1);
        let persisted = &snapshot.timed_approval_grants[0];
        assert_eq!(persisted.session_id, SessionId::new("valid-session"));
        assert_eq!(persisted.command, "cargo test --valid");
        assert_eq!(persisted.issued_at, snapshot_wall);
        assert_eq!(persisted.expires_at, issued_at + ChronoDuration::hours(1));
        assert_eq!(state.approvals.grant_count().await, 1);
    }

    #[tokio::test]
    async fn restore_discards_timed_grant_that_expired_during_downtime() {
        let source = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let issued_at = fixed_wall_now();
        let monotonic_issued_at = tokio::time::Instant::now();
        let approval = register_bash_approval_at(
            &source,
            "grant-source",
            "session-1",
            "cargo test",
            issued_at,
            monotonic_issued_at,
        )
        .await;
        source
            .approvals
            .resolve_with_grant_at(
                approval.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_issued_at,
            )
            .await
            .unwrap();
        let persisted = source
            .snapshot_at(
                issued_at + ChronoDuration::minutes(5),
                monotonic_issued_at + Duration::from_secs(5 * 60),
            )
            .await;

        let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let startup_wall = issued_at + ChronoDuration::minutes(31);
        let startup_monotonic = tokio::time::Instant::now();
        restored
            .restore_at(persisted, startup_wall, startup_monotonic)
            .await;
        let matching = register_bash_approval_at(
            &restored,
            "after-restore",
            "session-1",
            "cargo test",
            startup_wall,
            startup_monotonic,
        )
        .await;

        assert_eq!(matching.status, ApprovalStatus::Pending);
        assert_eq!(restored.approvals.grant_count().await, 0);
    }

    #[tokio::test]
    async fn restore_discards_semantically_impossible_timed_grants() {
        let issued_at = fixed_wall_now();
        let cases = [
            (
                "expiry equal to issuance",
                issued_at,
                issued_at,
                issued_at + ChronoDuration::minutes(1),
            ),
            (
                "expiry before issuance",
                issued_at,
                issued_at - ChronoDuration::nanoseconds(1),
                issued_at + ChronoDuration::minutes(1),
            ),
            (
                "original lifetime above 24 hours",
                issued_at,
                issued_at + ChronoDuration::hours(24) + ChronoDuration::nanoseconds(1),
                issued_at + ChronoDuration::minutes(1),
            ),
            (
                "remaining lifetime above 24 hours",
                issued_at,
                issued_at + ChronoDuration::hours(24) + ChronoDuration::nanoseconds(1),
                issued_at,
            ),
            (
                "startup clock before issuance",
                issued_at,
                issued_at + ChronoDuration::minutes(30),
                issued_at - ChronoDuration::nanoseconds(1),
            ),
        ];

        for (name, persisted_issued_at, persisted_expires_at, startup_wall) in cases {
            let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
            let mut persisted = restored
                .snapshot_at(startup_wall, tokio::time::Instant::now())
                .await;
            persisted.timed_approval_grants = vec![storage::PersistedApprovalGrant {
                session_id: SessionId::new("session-1"),
                command: "cargo test".to_string(),
                issued_at: persisted_issued_at,
                expires_at: persisted_expires_at,
            }];
            let startup_monotonic = tokio::time::Instant::now();

            restored
                .restore_at(persisted, startup_wall, startup_monotonic)
                .await;
            let matching = register_bash_approval_at(
                &restored,
                &format!("matching-{name}"),
                "session-1",
                "cargo test",
                startup_wall,
                startup_monotonic,
            )
            .await;

            assert_eq!(
                matching.status,
                ApprovalStatus::Pending,
                "{name} must not restore authority"
            );
            assert_eq!(
                restored.approvals.grant_count().await,
                0,
                "{name} must be discarded"
            );
        }
    }

    #[tokio::test]
    async fn restore_does_not_extend_a_grant_from_duplicate_persisted_entries() {
        let issued_at = fixed_wall_now();
        let startup_wall = issued_at + ChronoDuration::minutes(1);
        let startup_monotonic = tokio::time::Instant::now();
        let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let mut persisted = restored.snapshot_at(startup_wall, startup_monotonic).await;
        persisted.timed_approval_grants = vec![
            storage::PersistedApprovalGrant {
                session_id: SessionId::new("session-1"),
                command: "cargo test".to_string(),
                issued_at,
                expires_at: issued_at + ChronoDuration::minutes(10),
            },
            storage::PersistedApprovalGrant {
                session_id: SessionId::new("session-1"),
                command: "cargo test".to_string(),
                issued_at,
                expires_at: issued_at + ChronoDuration::minutes(20),
            },
        ];

        restored
            .restore_at(persisted, startup_wall, startup_monotonic)
            .await;

        let before_shorter_expiry = register_bash_approval_at(
            &restored,
            "before-shorter-expiry",
            "session-1",
            "cargo test",
            startup_wall,
            startup_monotonic + Duration::from_secs(9 * 60) - Duration::from_nanos(1),
        )
        .await;
        let at_shorter_expiry = register_bash_approval_at(
            &restored,
            "at-shorter-expiry",
            "session-1",
            "cargo test",
            startup_wall,
            startup_monotonic + Duration::from_secs(9 * 60),
        )
        .await;

        assert_eq!(
            before_shorter_expiry.status,
            ApprovalStatus::Approved { message: None }
        );
        assert_eq!(at_shorter_expiry.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn timed_grant_persistence_preserves_existing_session_and_pending_approval_snapshot() {
        let source = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let session_id = SessionId::new("persisted-session");
        source
            .sessions
            .get_or_register(&session_id, "/workspace/persisted-project", None)
            .await;
        source
            .sessions
            .set_status(
                &session_id,
                SessionStatus::Idle,
                None,
                Some("Persisted display name".to_string()),
            )
            .await;
        let pending = register_bash_approval_at(
            &source,
            "persisted-pending",
            "persisted-session",
            "cargo test",
            fixed_wall_now(),
            tokio::time::Instant::now(),
        )
        .await;
        let wall_now = fixed_wall_now() + ChronoDuration::minutes(1);
        let monotonic_now = tokio::time::Instant::now();

        let snapshot = source.snapshot_at(wall_now, monotonic_now).await;
        let restored = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        restored.restore_at(snapshot, wall_now, monotonic_now).await;

        let sessions = restored.sessions.list().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].project, "persisted-project");
        assert_eq!(sessions[0].stored_status, SessionStatus::Idle);
        assert_eq!(
            sessions[0].display_name.as_deref(),
            Some("Persisted display name")
        );
        let pending_after_restore = restored.approvals.list_pending().await;
        assert_eq!(pending_after_restore.len(), 1);
        assert_eq!(pending_after_restore[0].id, pending.id);
        assert_eq!(pending_after_restore[0].request_id, "persisted-pending");
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
    async fn approval_detail_json_cannot_close_its_script_element() {
        let state = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let attacker_command =
            "printf '</script><script>grantAuthority()</script>&>\u{2028}\u{2029}'";
        let approval = register_web_approval(
            &state,
            "malicious-json",
            Tool::Bash,
            serde_json::json!({"command": attacker_command}),
        )
        .await;
        let app = router(state);

        let (status, body) = get_json(&app, &format!("/approvals/{}", approval.id)).await;
        assert_eq!(status, AxumStatus::OK);
        let payload = body
            .split_once("<script type=\"application/json\" id=\"approval-data\">")
            .expect("detail page should contain approval JSON")
            .1
            .split_once("</script>\n<script>")
            .expect("approval JSON script should have a fixed closing delimiter")
            .0;

        assert!(
            !payload.to_ascii_lowercase().contains("</script"),
            "attacker-controlled JSON must not contain a literal closing script sequence: {payload}"
        );
        for character in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
            assert!(
                !payload.contains(character),
                "approval JSON must escape {character:?}: {payload}"
            );
        }
        let rendered: serde_json::Value =
            serde_json::from_str(payload).expect("escaped payload should remain valid JSON");
        assert_eq!(rendered["tool_input"]["command"], attacker_command);
    }

    #[tokio::test]
    async fn dashboard_timed_controls_render_only_for_bash_approvals() {
        let state = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let bash = register_web_approval(
            &state,
            "bash-controls",
            Tool::Bash,
            serde_json::json!({"command": "cargo test"}),
        )
        .await;
        let write = register_web_approval(
            &state,
            "write-controls",
            Tool::Write,
            serde_json::json!({"file_path": "/workspace/file", "content": "text"}),
        )
        .await;
        let app = router(state);

        let (_, dashboard) = get_json(&app, "/approvals").await;
        assert!(dashboard.contains(&format!("showTimedApproval('{}')", bash.id)));
        assert!(!dashboard.contains(&format!("showTimedApproval('{}')", write.id)));
    }

    #[tokio::test]
    async fn detail_timed_controls_render_only_for_bash_approvals() {
        let state = test_state_with_mode(ApprovalFeatureMode::Readwrite);
        let bash = register_web_approval(
            &state,
            "bash-detail-controls",
            Tool::Bash,
            serde_json::json!({"command": "cargo test"}),
        )
        .await;
        let write = register_web_approval(
            &state,
            "write-detail-controls",
            Tool::Write,
            serde_json::json!({"file_path": "/workspace/file", "content": "text"}),
        )
        .await;
        let app = router(state);

        let (_, bash_detail) = get_json(&app, &format!("/approvals/{}", bash.id)).await;
        let (_, write_detail) = get_json(&app, &format!("/approvals/{}", write.id)).await;
        assert!(bash_detail.contains(">Timed approve</button>"));
        assert!(!write_detail.contains(">Timed approve</button>"));
    }

    fn assert_resolution_script_checks_http_errors(
        name: &str,
        template: &str,
        action_start: &str,
        action_end: &str,
    ) {
        let action = template
            .split_once(action_start)
            .unwrap_or_else(|| panic!("{name} approval action is missing"))
            .1
            .split_once(action_end)
            .unwrap_or_else(|| panic!("{name} approval action has no end"))
            .0;

        assert!(
            action.contains(".ok"),
            "{name} must check response.ok before treating an approval as resolved"
        );
        assert!(
            action.contains("catch"),
            "{name} must retain an actionable approval error when the request fails"
        );
        assert!(
            action.contains("showError(")
                && template.contains("function showError")
                && template.contains("role=\"alert\""),
            "{name} must show the failed approval action in a persistent alert"
        );
    }

    #[test]
    fn dashboard_resolution_script_checks_http_errors() {
        assert_resolution_script_checks_http_errors(
            "dashboard",
            include_str!("../../templates/dashboard.html"),
            "window.approveRow = async function",
            "window.showTimedApproval = function",
        );
    }

    #[test]
    fn detail_resolution_script_checks_http_errors() {
        assert_resolution_script_checks_http_errors(
            "approval detail",
            include_str!("../../templates/approval_detail.html"),
            "window.doApprove = async function",
            "window.showTimedApproval = function",
        );
    }

    #[test]
    fn dashboard_polling_adds_timed_controls_only_for_bash() {
        let template = include_str!("../../templates/dashboard.html");
        let dynamic_rows = template
            .split_once("function createApprovalRows")
            .expect("dashboard should render approval rows from polling")
            .1
            .split_once("function renderApprovals")
            .expect("dynamic approval row function should have an end")
            .0;

        assert!(
            dynamic_rows.contains("tool_name")
                && dynamic_rows.contains("Bash")
                && dynamic_rows.contains("showTimedApproval"),
            "polled dashboard rows must gate timed controls on Bash approvals"
        );
    }

    #[test]
    fn all_approval_templates_expose_timed_and_one_time_keyboard_choices() {
        let templates = [
            ("dashboard", include_str!("../../templates/dashboard.html")),
            (
                "approval detail",
                include_str!("../../templates/approval_detail.html"),
            ),
            (
                "mobile queue",
                include_str!("../../templates/approval_queue.html"),
            ),
        ];

        for (name, template) in templates {
            assert!(
                template.contains("key === 'a'") || template.contains("event.key === 'a'"),
                "{name} lost the lowercase-a one-time approval shortcut"
            );
            assert!(
                template.contains("key === 'A'") || template.contains("event.key === 'A'"),
                "{name} has no timed-approval submenu"
            );
            for (key, duration) in [
                ("1", "30m"),
                ("2", "1h"),
                ("3", "2h"),
                ("4", "6h"),
                ("5", "24h"),
            ] {
                assert!(
                    template.contains(duration),
                    "{name} does not expose {duration}"
                );
                assert!(
                    template.contains(&format!("'{key}'")),
                    "{name} does not expose key {key}"
                );
            }
        }
    }

    #[tokio::test]
    async fn invalid_timed_approval_requests_return_bad_request_without_resolving() {
        let config = ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: None,
            default_approval_mode: SessionApprovalMode::Remote,
        };
        let state = AppState::new(config, NullNotifier, None);
        let approval = state
            .approvals
            .register(approvals::RegisterApproval {
                request_id: "request-invalid-timed".to_string(),
                session_id: SessionId::new("session-1"),
                session_display_name: "test session".to_string(),
                project: "/workspace".to_string(),
                tool: Tool::Bash,
                tool_input: serde_json::json!({"command": "cargo test"}),
                provider: "opencode".to_string(),
                request_type: RequestType::ToolUse,
                context: ApprovalContext {
                    workspace_roots: vec!["/workspace".to_string()],
                    hook_event_name: HookEventName::Other("test".to_string()),
                    extra: None,
                },
            })
            .await;
        let app = router(state.clone());

        for body in [
            r#"{"decision":"approve","message":null,"approve_for":"90m"}"#,
            r#"{"decision":"deny","message":null,"approve_for":"30m"}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("/api/v1/approvals/{}/resolve", approval.id))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), AxumStatus::BAD_REQUEST);
            assert_eq!(
                state.approvals.get(approval.id).await.unwrap().status,
                ApprovalStatus::Pending
            );
        }
    }

    #[tokio::test]
    async fn matching_incoming_approval_is_immediately_approved_without_notification() {
        let sends = Arc::new(AtomicUsize::new(0));
        let config = ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        };
        let state = AppState::new(
            config,
            CountingNotifier {
                sends: Arc::clone(&sends),
            },
            None,
        );
        let context = ApprovalContext {
            workspace_roots: vec!["/workspace".to_string()],
            hook_event_name: HookEventName::Other("test".to_string()),
            extra: None,
        };
        let monotonic_now = tokio::time::Instant::now();
        let first = state
            .approvals
            .register_at(
                approvals::RegisterApproval {
                    request_id: "request-1".to_string(),
                    session_id: SessionId::new("session-1"),
                    session_display_name: "test session".to_string(),
                    project: "/workspace".to_string(),
                    tool: Tool::Bash,
                    tool_input: serde_json::json!({"command": "cargo test"}),
                    provider: "opencode".to_string(),
                    request_type: RequestType::ToolUse,
                    context: context.clone(),
                },
                Utc::now(),
                monotonic_now,
            )
            .await;
        state
            .approvals
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .unwrap();

        let Json(response) = hooks::approval(
            axum::extract::State(state),
            Json(ApprovalRequest {
                id: "request-2".to_string(),
                session_id: SessionId::new("session-1"),
                session_display_name: "test session".to_string(),
                cwd: "/workspace".to_string(),
                tool: Tool::Bash,
                tool_input: serde_json::json!({"command": "cargo test"}),
                provider: "opencode".to_string(),
                request_type: RequestType::ToolUse,
                context,
            }),
        )
        .await;
        tokio::task::yield_now().await;

        assert_eq!(response.status, ApprovalStatus::Approved { message: None });
        assert_eq!(sends.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_approval_hook_notifies_only_from_the_authoritative_stored_request() {
        let sends = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent = Arc::new(tokio::sync::Notify::new());
        let config = ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        };
        let state = AppState::new(
            config,
            CapturingNotifier {
                sends: Arc::clone(&sends),
                sent: Arc::clone(&sent),
            },
            None,
        );
        let authoritative = ApprovalRequest {
            id: "duplicate-request".to_string(),
            session_id: SessionId::new("authoritative-session"),
            session_display_name: "authoritative display name".to_string(),
            cwd: "/workspace/authoritative-project".to_string(),
            tool: Tool::Bash,
            tool_input: serde_json::json!({"command": "authoritative command"}),
            provider: "opencode".to_string(),
            request_type: RequestType::ToolUse,
            context: ApprovalContext {
                workspace_roots: vec!["/workspace/authoritative-project".to_string()],
                hook_event_name: HookEventName::Other("test".to_string()),
                extra: None,
            },
        };
        let replay = ApprovalRequest {
            id: authoritative.id.clone(),
            session_id: SessionId::new("replay-session"),
            session_display_name: "replay display name".to_string(),
            cwd: "/workspace/replay-project".to_string(),
            tool: Tool::Write,
            tool_input: serde_json::json!({
                "file_path": "/workspace/replay-project/conflicting",
                "content": "replay content"
            }),
            provider: "conflicting-provider".to_string(),
            request_type: RequestType::ToolUse,
            context: ApprovalContext {
                workspace_roots: vec!["/workspace/replay-project".to_string()],
                hook_event_name: HookEventName::Other("conflicting".to_string()),
                extra: None,
            },
        };

        let Json(first_response) =
            hooks::approval(axum::extract::State(state.clone()), Json(authoritative)).await;
        sent.notified().await;
        let Json(replay_response) =
            hooks::approval(axum::extract::State(state), Json(replay)).await;
        tokio::task::yield_now().await;

        assert_eq!(replay_response.id, first_response.id);
        assert_eq!(
            *sends
                .lock()
                .expect("captured notifications lock should not be poisoned"),
            vec![CapturedNotification {
                title: "Agent Hub (approval)".to_string(),
                message: "[authoritative-project] Bash — {\"command\":\"authoritative command\"}"
                    .to_string(),
                url: Some(format!(
                    "http://localhost:8080/approvals/{}",
                    first_response.id
                )),
            }],
            "a duplicate request must not notify again or use conflicting replay fields"
        );
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

    /// A Waiting session whose question has been resolved must be eligible for
    /// zombie reset (first_pending_for_session returns None after resolve).
    #[tokio::test]
    async fn waiting_session_becomes_zombie_after_question_resolved() {
        use protocol::QuestionInfo;
        use questions::{QuestionRegistry, QuestionStatus, RegisterQuestion};

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

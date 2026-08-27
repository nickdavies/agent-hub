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

use axum::extract::rejection::JsonRejection;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::RwLock;
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
    use axum::http::{Request, StatusCode as AxumStatus};
    use chrono::Utc;
    use config::{ApprovalFeatureMode, AuthMode, ServerConfig, Token};
    use notifier::{Notifier, NotifyError, NullNotifier};
    use protocol::{
        ApprovalContext, ApprovalGrantDuration, ApprovalRequest, HookEventName, RequestType, Tool,
    };
    use sessions::SessionApprovalMode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt; // for oneshot

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

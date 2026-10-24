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

use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use tokio::sync::RwLock;
use tower_sessions::{MemoryStore, SessionManagerLayer};
use uuid::Uuid;

use crate::error::AppError;
use crate::mcp;
use approvals::{ApprovalRegistry, ApprovalStatus};
use config::{
    ApprovalFeatureMode, AuthMode, NotifyConfig, NotifyConfigUpdate, Role, ServerConfig,
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
    WhoAmIResponse,
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

    // Routes any authenticated principal may call. Agents raise, monitor and
    // cancel requests here; the resolve handlers enforce per-decision roles.
    let mut agent_api = Router::new()
        .route("/hooks/stop", post(hooks::stop::<N>))
        .route("/hooks/notification", post(hooks::notification::<N>))
        .route("/hooks/session-end", post(hooks::session_end::<N>))
        .route("/hooks/status", post(hooks::status::<N>))
        .route("/whoami", get(handle_whoami))
        .nest_service("/mcp", mcp_service);

    // Routes only reviewers (humans, CLI) may call.
    let mut reviewer_api = Router::new()
        .route("/presence", post(handle_presence_update::<N>))
        .route("/sessions", get(handle_list_sessions::<N>))
        .route("/sessions/{id}", put(handle_update_session::<N>))
        .route(
            "/config",
            get(handle_get_config::<N>).put(handle_put_config::<N>),
        );

    // Mount approval API routes only when approval mode is not disabled
    if state.config.approval_mode != ApprovalFeatureMode::Disabled {
        agent_api = agent_api
            .route("/hooks/approval", post(hooks::approval::<N>))
            .route("/hooks/question", post(hooks::question::<N>))
            .route("/approvals/{id}", get(handle_get_approval::<N>))
            .route("/approvals/{id}/wait", get(handle_approval_wait::<N>))
            .route(
                "/approvals/{id}/resolve",
                post(handle_approval_resolve::<N>),
            )
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
        reviewer_api = reviewer_api
            .route("/approvals/pending", get(handle_list_pending::<N>))
            .route("/questions/pending", get(handle_list_questions::<N>));
    }

    let api_v1 = agent_api.merge(reviewer_api.route_layer(from_fn(auth::require_reviewer)));

    let api_v1 = if state.config.auth_mode == AuthMode::None {
        api_v1
            .layer(Extension(auth::Principal::anonymous()))
            .with_state(state.clone())
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
        // External request content is shown only on the approval page.
        if approval.request_type.is_external() {
            return EffectiveSessionStatus::Waiting {
                reason: Some(format!("Pending approval: {}", approval.tool)),
            };
        }
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

async fn handle_whoami(Extension(principal): Extension<auth::Principal>) -> Json<WhoAmIResponse> {
    Json(WhoAmIResponse {
        label: principal.label,
        role: principal.role,
    })
}

/// Agents may only cancel requests; resolving them requires a reviewer.
fn ensure_can_resolve(principal: &auth::Principal, is_cancel: bool) -> Result<(), AppError> {
    if is_cancel || principal.role == Role::Reviewer {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "'{}' has role {:?} and may only cancel",
        principal.label, principal.role
    )))
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
    Extension(principal): Extension<auth::Principal>,
    Json(req): Json<ApprovalResolveRequest>,
) -> Result<Json<approvals::Approval>, AppError> {
    ensure_can_resolve(&principal, req.decision == ApprovalDecision::Cancel)?;

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
    Extension(principal): Extension<auth::Principal>,
    Json(req): Json<QuestionResolveRequest>,
) -> Result<Json<questions::PendingQuestion>, AppError> {
    ensure_can_resolve(&principal, req.decision == QuestionDecision::Cancel)?;

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
    use config::{ApprovalFeatureMode, AuthMode, ServerConfig, Token};
    use notifier::NullNotifier;
    use sessions::SessionApprovalMode;
    use tower::ServiceExt; // for oneshot

    /// Config with auth disabled (no Bearer tokens needed).
    fn no_auth_config(approval_mode: ApprovalFeatureMode) -> ServerConfig {
        ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        }
    }

    /// Build a test router with auth disabled (no Bearer tokens needed).
    fn test_app_with_mode(approval_mode: ApprovalFeatureMode) -> Router {
        router(AppState::new(
            no_auth_config(approval_mode),
            NullNotifier,
            None,
        ))
    }

    /// Notifier that forwards each push message to a channel.
    struct RecordingNotifier(tokio::sync::mpsc::UnboundedSender<String>);

    impl Notifier for RecordingNotifier {
        fn name(&self) -> &'static str {
            "recording"
        }

        async fn send(
            &self,
            _title: &str,
            message: &str,
            _url: Option<&str>,
        ) -> Result<(), notifier::NotifyError> {
            let _ = self.0.send(message.to_string());
            Ok(())
        }
    }

    /// Build a no-auth test router whose push messages arrive on the receiver.
    fn test_app_recording() -> (Router, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state = AppState::new(
            no_auth_config(ApprovalFeatureMode::Readwrite),
            RecordingNotifier(tx),
            None,
        );
        (router(state), rx)
    }

    /// Wait briefly for the next push; `None` if none was sent.
    async fn next_push(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Option<String> {
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .ok()
            .flatten()
    }

    fn test_app() -> Router {
        test_app_with_mode(ApprovalFeatureMode::Readwrite)
    }

    const AGENT_TOKEN: &str = "agent-secret";
    const REVIEWER_TOKEN: &str = "reviewer-secret";

    /// Build a test router with token auth: one agent and one reviewer token.
    fn test_app_with_tokens() -> Router {
        let config = ServerConfig {
            auth_mode: AuthMode::Token,
            tokens: vec![
                Token {
                    label: "agent".into(),
                    secret: protocol::Secret::new(AGENT_TOKEN),
                    role: Role::Agent,
                },
                Token {
                    label: "cli".into(),
                    secret: protocol::Secret::new(REVIEWER_TOKEN),
                    role: Role::Reviewer,
                },
            ],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: Some("http://localhost:8080".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        };
        router(AppState::new(config, NullNotifier, None))
    }

    /// Helper: send a request with an optional bearer token and JSON body;
    /// return (status_code, body_string).
    async fn send(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (AxumStatus, String) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
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
    async fn mobile_approval_queue_requires_web_auth() {
        let app = test_app_with_tokens();

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

    // ---------------------------------------------------------------
    // Token roles — agents raise/monitor/cancel, reviewers resolve
    // ---------------------------------------------------------------

    /// Register an approval with the given token and return its id.
    async fn register_approval(app: &Router, token: &str, request_id: &str) -> String {
        let body = format!(
            r#"{{
                "id": "{request_id}",
                "session_id": "ses_roles",
                "session_display_name": "Roles",
                "cwd": "/tmp/proj",
                "tool_name": "Bash",
                "tool_input": {{"command": "ls"}},
                "provider": "opencode",
                "request_type": "tool_use",
                "context": {{"workspace_roots": ["/tmp/proj"], "hook_event_name": "permission.ask"}}
            }}"#
        );
        let (status, body) = send(
            app,
            "POST",
            "/api/v1/hooks/approval",
            Some(token),
            Some(&body),
        )
        .await;
        assert_eq!(
            status,
            AxumStatus::OK,
            "approval registration failed: {body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// Register a question with the given token and return its id.
    async fn register_question(app: &Router, token: &str, request_id: &str) -> String {
        let body = format!(
            r#"{{
                "id": "{request_id}",
                "session_id": "ses_roles",
                "session_display_name": "Roles",
                "cwd": "/tmp/proj",
                "question_request_id": "oc-{request_id}",
                "questions": [{{"question":"Continue?","header":"Plan","options":[{{"label":"Yes","description":"Go"}}]}}],
                "provider": "opencode"
            }}"#
        );
        let (status, body) = send(
            app,
            "POST",
            "/api/v1/hooks/question",
            Some(token),
            Some(&body),
        )
        .await;
        assert_eq!(
            status,
            AxumStatus::OK,
            "question registration failed: {body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn missing_or_invalid_token_is_unauthorized() {
        let app = test_app_with_tokens();

        let (status, _) = send(&app, "GET", "/api/v1/whoami", None, None).await;
        assert_eq!(status, AxumStatus::UNAUTHORIZED);

        let (status, _) = send(&app, "GET", "/api/v1/whoami", Some("wrong"), None).await;
        assert_eq!(status, AxumStatus::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn whoami_reports_token_role() {
        let app = test_app_with_tokens();

        let (status, body) = send(&app, "GET", "/api/v1/whoami", Some(AGENT_TOKEN), None).await;
        assert_eq!(status, AxumStatus::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["label"], "agent");
        assert_eq!(v["role"], "agent");

        let (_, body) = send(&app, "GET", "/api/v1/whoami", Some(REVIEWER_TOKEN), None).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["role"], "reviewer");
    }

    #[tokio::test]
    async fn agent_cannot_resolve_approval_but_can_cancel() {
        let app = test_app_with_tokens();
        let id = register_approval(&app, AGENT_TOKEN, "req-agent-1").await;
        let resolve = format!("/api/v1/approvals/{id}/resolve");

        for decision in ["approve", "deny"] {
            let body = format!(r#"{{"decision":"{decision}"}}"#);
            let (status, _) = send(&app, "POST", &resolve, Some(AGENT_TOKEN), Some(&body)).await;
            assert_eq!(status, AxumStatus::FORBIDDEN, "agent must not {decision}");
        }

        let (status, body) = send(
            &app,
            "GET",
            &format!("/api/v1/approvals/{id}"),
            Some(AGENT_TOKEN),
            None,
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["status"]["type"], "pending",
            "approval must still be pending: {body}"
        );

        let (status, _) = send(
            &app,
            "POST",
            &resolve,
            Some(AGENT_TOKEN),
            Some(r#"{"decision":"cancel"}"#),
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
    }

    #[tokio::test]
    async fn reviewer_can_approve() {
        let app = test_app_with_tokens();
        let id = register_approval(&app, AGENT_TOKEN, "req-agent-2").await;

        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/approvals/{id}/resolve"),
            Some(REVIEWER_TOKEN),
            Some(r#"{"decision":"approve"}"#),
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
    }

    #[tokio::test]
    async fn agent_cannot_answer_question_but_can_cancel() {
        let app = test_app_with_tokens();
        let id = register_question(&app, AGENT_TOKEN, "req-agent-q1").await;
        let resolve = format!("/api/v1/questions/{id}/resolve");

        for body in [
            r#"{"decision":"answer","answers":[["Yes"]]}"#,
            r#"{"decision":"reject"}"#,
        ] {
            let (status, _) = send(&app, "POST", &resolve, Some(AGENT_TOKEN), Some(body)).await;
            assert_eq!(
                status,
                AxumStatus::FORBIDDEN,
                "agent must not resolve with {body}"
            );
        }

        let (status, _) = send(
            &app,
            "POST",
            &resolve,
            Some(AGENT_TOKEN),
            Some(r#"{"decision":"cancel"}"#),
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
    }

    #[tokio::test]
    async fn reviewer_can_answer_question() {
        let app = test_app_with_tokens();
        let id = register_question(&app, AGENT_TOKEN, "req-agent-q2").await;

        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/questions/{id}/resolve"),
            Some(REVIEWER_TOKEN),
            Some(r#"{"decision":"answer","answers":[["Yes"]]}"#),
        )
        .await;
        assert_eq!(status, AxumStatus::OK);
    }

    #[tokio::test]
    async fn agent_is_forbidden_from_reviewer_routes() {
        let app = test_app_with_tokens();

        for (method, path, body) in [
            ("GET", "/api/v1/approvals/pending", None),
            ("GET", "/api/v1/questions/pending", None),
            ("GET", "/api/v1/sessions", None),
            (
                "PUT",
                "/api/v1/sessions/ses_roles",
                Some(r#"{"stop_enabled":false}"#),
            ),
            ("GET", "/api/v1/config", None),
            ("PUT", "/api/v1/config", Some(r#"{"stop_enabled":false}"#)),
            ("POST", "/api/v1/presence", Some(r#"{"state":"away"}"#)),
        ] {
            let (status, _) = send(&app, method, path, Some(AGENT_TOKEN), body).await;
            assert_eq!(status, AxumStatus::FORBIDDEN, "agent {method} {path}");

            let (status, body) = send(&app, method, path, Some(REVIEWER_TOKEN), body).await;
            assert_ne!(
                status,
                AxumStatus::FORBIDDEN,
                "reviewer {method} {path}: {body}"
            );
            assert_ne!(
                status,
                AxumStatus::UNAUTHORIZED,
                "reviewer {method} {path}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn auth_none_grants_reviewer_access() {
        let app = test_app();

        let (status, body) = send(&app, "GET", "/api/v1/whoami", None, None).await;
        assert_eq!(status, AxumStatus::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["role"], "reviewer");
    }

    // ---------------------------------------------------------------
    // External requests — content never leaves the approval page
    // ---------------------------------------------------------------

    const SECRET_TITLE: &str = "Lunch with secret-attendee";

    fn external_body(request_id: &str, display: &str) -> String {
        format!(
            r#"{{
                "id": "{request_id}",
                "session_id": "pipeline:calendar",
                "session_display_name": "calendar",
                "cwd": "/pipelines/calendar",
                "tool_name": "calendar.create_event",
                "tool_input": {{"title": "{SECRET_TITLE}", "start": "2026-10-24T12:00:00Z", "attendees": ["a@example.com"]}},
                "provider": "assistant-pipelines",
                "request_type": "external",
                "context": {{"workspace_roots": [], "hook_event_name": "pipeline.proposal"}}
                {display}
            }}"#
        )
    }

    const CALENDAR_DISPLAY: &str = r#", "display": {
        "heading": "Create calendar event",
        "fields": [
            {"key": "title", "label": "Title"},
            {"key": "start", "label": "Start", "format": "datetime"},
            {"key": "attendees", "format": "list"}
        ],
        "summary": ["title", "start"]
    }"#;

    #[tokio::test]
    async fn external_push_omits_content() {
        let (app, mut pushes) = test_app_recording();

        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&external_body("ext-1", CALENDAR_DISPLAY)),
        )
        .await;
        assert_eq!(status, AxumStatus::OK, "{body}");

        let push = next_push(&mut pushes).await.expect("push should be sent");
        assert_eq!(push, "[calendar] calendar.create_event — approval pending");
    }

    #[tokio::test]
    async fn tool_use_push_still_includes_input() {
        let (app, mut pushes) = test_app_recording();
        register_approval(&app, REVIEWER_TOKEN, "tool-1").await;

        let push = next_push(&mut pushes).await.expect("push should be sent");
        assert!(push.contains(r#"{"command":"ls"}"#), "got: {push}");
    }

    #[tokio::test]
    async fn reposting_existing_request_does_not_push_again() {
        let (app, mut pushes) = test_app_recording();
        let first = register_approval(&app, REVIEWER_TOKEN, "dup-1").await;
        assert!(next_push(&mut pushes).await.is_some());

        let second = register_approval(&app, REVIEWER_TOKEN, "dup-1").await;
        assert_eq!(first, second, "idempotent register should return same id");
        assert_eq!(next_push(&mut pushes).await, None);
    }

    #[tokio::test]
    async fn display_rejected_on_unknown_key_or_non_external() {
        let app = test_app();

        let unknown_key = external_body("ext-bad", r#", "display": {"fields": [{"key": "nope"}]}"#);
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&unknown_key),
        )
        .await;
        assert_eq!(status, AxumStatus::BAD_REQUEST, "{body}");

        let unknown_summary = external_body(
            "ext-bad-summary",
            r#", "display": {"fields": [], "summary": ["nope"]}"#,
        );
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&unknown_summary),
        )
        .await;
        assert_eq!(status, AxumStatus::BAD_REQUEST, "{body}");

        let tool_use = external_body("tool-bad", CALENDAR_DISPLAY).replace(
            r#""request_type": "external""#,
            r#""request_type": "tool_use""#,
        );
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&tool_use),
        )
        .await;
        assert_eq!(status, AxumStatus::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn external_session_status_omits_content() {
        let app = test_app();
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&external_body("ext-status", "")),
        )
        .await;
        assert_eq!(status, AxumStatus::OK, "{body}");

        let (_, body) = get_json(&app, "/api/v1/sessions").await;
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let sess = sessions
            .iter()
            .find(|s| s["session_id"] == "pipeline:calendar")
            .unwrap();
        assert_eq!(
            sess["status"]["reason"],
            "Pending approval: calendar.create_event"
        );
    }

    #[tokio::test]
    async fn resolved_external_request_drops_content_but_keeps_decision() {
        let app = test_app();
        let (_, body) = send(
            &app,
            "POST",
            "/api/v1/hooks/approval",
            None,
            Some(&external_body("ext-resolve", CALENDAR_DISPLAY)),
        )
        .await;
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let (_, body) = get_json(&app, &format!("/api/v1/approvals/{id}")).await;
        assert!(body.contains(SECRET_TITLE), "pending request shows content");

        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/approvals/{id}/resolve"),
            None,
            Some(r#"{"decision":"approve","message":null}"#),
        )
        .await;
        assert_eq!(status, AxumStatus::OK);

        let (status, body) = get_json(&app, &format!("/api/v1/approvals/{id}/wait")).await;
        assert_eq!(status, AxumStatus::OK);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["type"],
            "approved"
        );

        let (_, body) = get_json(&app, &format!("/api/v1/approvals/{id}")).await;
        let approval: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(approval["tool_input"].is_null(), "got: {body}");
        assert!(approval.get("display").is_none(), "got: {body}");
    }

    /// Approval JSON embedded in the dashboard and detail pages must not be
    /// able to close its <script> tag.
    #[tokio::test]
    async fn embedded_approval_json_cannot_close_script_tag() {
        let app = test_app();
        let payload = "</script><script>alert(1)</script>";
        let body = format!(
            r#"{{
                "id": "xss-1",
                "session_id": "ses_xss",
                "session_display_name": "x",
                "cwd": "/tmp/x",
                "tool_name": "Bash",
                "tool_input": {{"command": "{payload}"}},
                "provider": "claude-code",
                "request_type": "tool_use",
                "context": {{"workspace_roots": [], "hook_event_name": "PreToolUse"}}
            }}"#
        );
        let (_, body) = send(&app, "POST", "/api/v1/hooks/approval", None, Some(&body)).await;
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        for path in [format!("/approvals/{id}"), "/approvals".to_string()] {
            let (status, html) = get_json(&app, &path).await;
            assert_eq!(status, AxumStatus::OK, "{path}");
            assert!(!html.contains(payload), "{path} embeds raw payload");
            assert!(
                html.contains(r"\u003c/script\u003e\u003cscript\u003ealert(1)"),
                "{path} should embed escaped payload"
            );
        }
    }
}

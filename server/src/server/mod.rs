pub mod approvals;
pub mod auth;
pub mod config;
pub mod hooks;
pub mod notifier;
pub mod oauth;
pub mod presence;
pub mod pushover;
pub mod sessions;
pub mod storage;
pub mod web;
pub mod webhook;

use std::sync::Arc;
use std::time::Duration;

use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
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
use sessions::{
    EffectiveSessionStatus, SessionApprovalMode, SessionConfigUpdate, SessionRegistry,
    SessionStatus,
};

pub struct AppState<N: Notifier> {
    pub config: Arc<ServerConfig>,
    pub presence: Arc<Presence>,
    pub sessions: Arc<SessionRegistry>,
    pub notifier: Arc<N>,
    pub notify_config: SharedNotifyConfig,
    pub pending: Arc<PendingNotifications>,
    pub approvals: Arc<ApprovalRegistry>,
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
            .route("/approvals/pending", get(handle_list_pending::<N>))
            .route("/approvals/{id}", get(handle_get_approval::<N>))
            .route("/approvals/{id}/wait", get(handle_approval_wait::<N>))
            .route(
                "/approvals/{id}/resolve",
                post(handle_approval_resolve::<N>),
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
/// with server-side knowledge (pending approvals).
pub(crate) fn resolve_effective_status(
    stored: SessionStatus,
    waiting_reason: Option<&str>,
    pending_approval: Option<&approvals::Approval>,
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
        let reason = format!("Pending approval: {} — {}", approval.tool_name, truncated);
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
        _ => unreachable!(),
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
        let status = resolve_effective_status(
            s.stored_status,
            s.waiting_reason.as_deref(),
            pending.as_ref(),
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
    state
        .sessions
        .update_config(&id, &update)
        .await
        .ok_or(crate::error::AppError::SessionNotFound(id))
        .map(Json)
}

#[derive(serde::Serialize)]
struct ConfigResponse {
    #[serde(flatten)]
    notify: NotifyConfig,
    presence: presence::PresenceState,
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

#[derive(Serialize)]
struct ApprovalWaitResponse {
    #[serde(flatten)]
    status: ApprovalStatus,
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

#[derive(Deserialize)]
struct ApprovalResolveRequest {
    decision: ApprovalDecision,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApprovalDecision {
    Approve,
    Deny,
    Cancel,
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

#[derive(Serialize)]
struct ApprovalModeResponse {
    approval_mode: SessionApprovalMode,
}

/// GET /api/v1/sessions/{id}/approval-mode
async fn handle_get_approval_mode<N: Notifier>(
    axum::extract::State(state): axum::extract::State<AppState<N>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<ApprovalModeResponse>, AppError> {
    let cfg = state
        .sessions
        .get_config(&id)
        .await
        .ok_or(AppError::SessionNotFound(id))?;
    Ok(Json(ApprovalModeResponse {
        approval_mode: cfg.approval_mode,
    }))
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
    use config::{ApprovalFeatureMode, AuthMode, ServerConfig};
    use notifier::NullNotifier;
    use tower::ServiceExt; // for oneshot

    /// Build a test router with auth disabled (no Bearer tokens needed).
    fn test_app() -> Router {
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
        let state = AppState::new(config, NullNotifier, None);
        router(state)
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
        assert_eq!(status, AxumStatus::OK, "opencode status report should be accepted");
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
        assert_eq!(status, AxumStatus::OK, "approval should be registered: {body}");

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
}

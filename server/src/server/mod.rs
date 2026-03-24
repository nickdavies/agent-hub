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
use config::{ApprovalFeatureMode, NotifyConfig, NotifyConfigUpdate, ServerConfig, SharedNotifyConfig};
use hooks::PendingNotifications;
use notifier::Notifier;
use oauth::OAuthManager;
use presence::{Presence, PresenceUpdate};
use sessions::{SessionApprovalMode, SessionConfigUpdate, SessionRegistry};

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
        Arc::clone(&state.notify_config),
        Arc::clone(&state.presence),
    );

    let mut api_v1 = Router::new()
        .route("/hooks/stop", post(hooks::stop::<N>))
        .route("/hooks/notification", post(hooks::notification::<N>))
        .route("/hooks/session-end", post(hooks::session_end::<N>))
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

    let api_v1 = api_v1
        .layer(from_fn_with_state(state.clone(), auth::require_auth::<N>))
        .with_state(state.clone());

    let public = Router::new().route("/health", get(health));

    let mut app = Router::new().nest("/api/v1", api_v1).merge(public);

    // Mount web UI and OAuth routes when approval mode is not disabled
    if state.config.approval_mode != ApprovalFeatureMode::Disabled {
        // Auth routes (public, no auth required)
        let auth_routes = Router::new()
            .route("/auth/login", get(web::login_page::<N>))
            .route("/auth/start/{provider}", get(oauth::start_auth::<N>))
            .route("/auth/callback/{provider}", get(oauth::callback::<N>))
            .route("/auth/logout", post(oauth::logout))
            .with_state(state.clone());

        // Web UI routes (session auth enforced by middleware)
        let web_routes = Router::new()
            .route("/approvals", get(web::dashboard::<N>))
            .route("/approvals/{id}", get(web::approval_detail::<N>))
            .route(
                "/approvals/{id}/resolve",
                post(web::resolve_approval::<N>),
            )
            .route(
                "/approvals/toggle-mode/{session_id}",
                post(web::toggle_approval_mode::<N>),
            )
            .layer(from_fn(auth::require_web_auth))
            .with_state(state.clone());

        app = app.merge(auth_routes).merge(web_routes);
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
    Json(state.sessions.list().await)
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

    // If already resolved, return immediately
    if rx.borrow().is_resolved() {
        let status = rx.borrow().clone();
        return Ok((axum::http::StatusCode::OK, Json(ApprovalWaitResponse { status })));
    }

    // Long-poll: wait up to 55s for a change
    let result = tokio::time::timeout(Duration::from_secs(55), rx.changed()).await;

    let status = rx.borrow().clone();
    if result.is_ok() && status.is_resolved() {
        Ok((axum::http::StatusCode::OK, Json(ApprovalWaitResponse { status })))
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
        ApprovalDecision::Cancel => ApprovalStatus::Superseded,
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
        .ok_or_else(|| AppError::SessionNotFound(id))?;
    Ok(Json(ApprovalModeResponse {
        approval_mode: cfg.approval_mode,
    }))
}

impl<N: Notifier> AppState<N> {
    pub fn new(
        server_config: ServerConfig,
        notifier: N,
        oauth: Option<OAuthManager>,
    ) -> Self {
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

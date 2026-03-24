pub mod auth;
pub mod config;
pub mod hooks;
pub mod presence;
pub mod pushover;
pub mod sessions;
pub mod storage;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::RwLock;

use crate::mcp;
use config::{NotifyConfig, NotifyConfigUpdate, ServerConfig, SharedNotifyConfig};
use hooks::PendingNotifications;
use presence::{Presence, PresenceUpdate};
use pushover::PushoverClient;
use sessions::{SessionConfigUpdate, SessionRegistry};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub presence: Arc<Presence>,
    pub sessions: Arc<SessionRegistry>,
    pub pushover: Arc<PushoverClient>,
    pub notify_config: SharedNotifyConfig,
    pub pending: Arc<PendingNotifications>,
}

pub fn router(state: AppState) -> Router {
    let mcp_service = mcp::service(state.clone());

    let authed = Router::new()
        .route("/hooks/stop", post(hooks::stop))
        .route("/hooks/notification", post(hooks::notification))
        .route("/hooks/session-end", post(hooks::session_end))
        .route("/presence", post(handle_presence_update))
        .route("/sessions", get(handle_list_sessions))
        .route("/sessions/{id}", put(handle_update_session))
        .route("/config", get(handle_get_config).put(handle_put_config))
        .nest_service("/mcp", mcp_service)
        .layer(from_fn_with_state(state.clone(), auth::require_auth))
        .with_state(state.clone());

    let public = Router::new().route("/health", get(health));

    Router::new()
        .merge(authed)
        .merge(public)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}

async fn handle_presence_update(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<PresenceUpdate>,
) -> axum::http::StatusCode {
    state.presence.set(body.state).await;
    tracing::info!(state = ?body.state, "presence updated");
    axum::http::StatusCode::OK
}

async fn handle_list_sessions(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<Vec<sessions::SessionView>> {
    Json(state.sessions.list().await)
}

async fn handle_update_session(
    axum::extract::State(state): axum::extract::State<AppState>,
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

async fn handle_get_config(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<ConfigResponse> {
    let notify = state.notify_config.read().await.clone();
    let presence = state.presence.get().await;
    Json(ConfigResponse { notify, presence })
}

async fn handle_put_config(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(update): Json<NotifyConfigUpdate>,
) -> Json<NotifyConfig> {
    let mut cfg = state.notify_config.write().await;
    cfg.apply(update);
    Json(cfg.clone())
}

impl AppState {
    pub fn new(server_config: ServerConfig) -> Self {
        let pushover = PushoverClient::new(
            server_config.pushover_token.clone(),
            server_config.pushover_user.clone(),
        );
        let presence = Presence::new(server_config.presence_ttl_secs);
        let sessions = SessionRegistry::new(server_config.session_ttl_secs);
        let notify_config = NotifyConfig::with_delay(server_config.notification_delay_secs);

        Self {
            config: Arc::new(server_config),
            presence: Arc::new(presence),
            sessions: Arc::new(sessions),
            pushover: Arc::new(pushover),
            notify_config: Arc::new(RwLock::new(notify_config)),
            pending: Arc::new(PendingNotifications::new()),
        }
    }

    /// Capture current state for persistence.
    pub async fn snapshot(&self) -> storage::PersistedState {
        storage::PersistedState {
            sessions: self.sessions.snapshot().await,
            notify_config: Some(self.notify_config.read().await.clone()),
            presence: Some(self.presence.raw_state().await),
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
    }
}

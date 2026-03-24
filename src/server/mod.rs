pub mod auth;
pub mod config;
pub mod hooks;
pub mod notifier;
pub mod presence;
pub mod pushover;
pub mod sessions;
pub mod storage;
pub mod webhook;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use tokio::sync::RwLock;

use crate::mcp;
use config::{NotifyConfig, NotifyConfigUpdate, ServerConfig, SharedNotifyConfig};
use hooks::PendingNotifications;
use notifier::Notifier;
use presence::{Presence, PresenceUpdate};
use sessions::{SessionConfigUpdate, SessionRegistry};

pub struct AppState<N: Notifier> {
    pub config: Arc<ServerConfig>,
    pub presence: Arc<Presence>,
    pub sessions: Arc<SessionRegistry>,
    pub notifier: Arc<N>,
    pub notify_config: SharedNotifyConfig,
    pub pending: Arc<PendingNotifications>,
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
        }
    }
}

pub fn router<N: Notifier>(state: AppState<N>) -> Router {
    let mcp_service = mcp::service(
        Arc::clone(&state.sessions),
        Arc::clone(&state.notify_config),
        Arc::clone(&state.presence),
    );

    let authed = Router::new()
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
        .nest_service("/mcp", mcp_service)
        .layer(from_fn_with_state(state.clone(), auth::require_auth::<N>))
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

impl<N: Notifier> AppState<N> {
    pub fn new(server_config: ServerConfig, notifier: N) -> Self {
        let presence = Presence::new(server_config.presence_ttl_secs);
        let sessions = SessionRegistry::new(server_config.session_ttl_secs);
        let notify_config = NotifyConfig::with_delay(server_config.notification_delay_secs);

        Self {
            config: Arc::new(server_config),
            presence: Arc::new(presence),
            sessions: Arc::new(sessions),
            notifier: Arc::new(notifier),
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

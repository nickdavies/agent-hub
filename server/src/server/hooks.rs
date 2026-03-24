use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::AppState;
use super::approvals::ApprovalStatus;
use super::notifier::Notifier;
use crate::server::presence::PresenceState;

/// Common fields from hook payloads. Serde ignores unknown fields by default.
#[derive(Deserialize)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub message: Option<String>,
}

/// Tracks pending delayed notifications so they can be cancelled.
pub struct PendingNotifications {
    pending: RwLock<HashMap<String, CancellationToken>>,
}

impl PendingNotifications {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Register a cancellation token for a session. Cancels any existing pending notification.
    async fn insert(&self, session_id: &str) -> CancellationToken {
        let mut map = self.pending.write().await;
        // Cancel any existing pending notification for this session
        if let Some(old) = map.remove(session_id) {
            old.cancel();
        }
        let token = CancellationToken::new();
        map.insert(session_id.to_owned(), token.clone());
        token
    }

    /// Cancel and remove a pending notification for a session.
    pub async fn cancel(&self, session_id: &str) {
        let mut map = self.pending.write().await;
        if let Some(token) = map.remove(session_id) {
            token.cancel();
            info!(session_id, "pending notification cancelled");
        }
    }

    /// Remove the token (called when the delayed notification fires successfully).
    async fn remove(&self, session_id: &str) {
        self.pending.write().await.remove(session_id);
    }
}

/// POST /hooks/stop
pub async fn stop<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    let (session_id, cwd) = match extract_session(&payload) {
        Some(v) => v,
        None => {
            warn!("stop hook: missing session_id or cwd");
            return StatusCode::OK;
        }
    };

    // Cancel any pending permission notification before sending the stop notification
    state.pending.cancel(&session_id).await;

    let project = state.sessions.get_or_register(&session_id, &cwd).await;
    let session_cfg = state.sessions.get_config(&session_id).await;
    let presence = state.presence.get().await;
    let global = state.notify_config.read().await;

    let should_notify = presence != PresenceState::Present
        && global.stop_enabled
        && session_cfg.as_ref().is_some_and(|c| c.stop_enabled);

    if should_notify {
        let notifier = Arc::clone(&state.notifier);
        let title = "Claude Code".to_string();
        let message = format!("[{project}] Claude finished");
        tokio::spawn(async move {
            fire_and_forget(&*notifier, &title, &message, None).await;
        });
    } else {
        info!(
            session_id,
            present = ?presence,
            "stop hook: notification suppressed"
        );
    }

    state.sessions.deregister(&session_id).await;

    StatusCode::OK
}

/// POST /hooks/notification
pub async fn notification<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    let (session_id, cwd) = match extract_session(&payload) {
        Some(v) => v,
        None => {
            warn!("notification hook: missing session_id or cwd");
            return StatusCode::OK;
        }
    };

    let project = state.sessions.get_or_register(&session_id, &cwd).await;
    let session_cfg = state.sessions.get_config(&session_id).await;
    let presence = state.presence.get().await;
    let global = state.notify_config.read().await;

    let should_notify = presence != PresenceState::Present
        && global.permission_enabled
        && session_cfg.as_ref().is_some_and(|c| c.permission_enabled);

    if !should_notify {
        info!(
            session_id,
            present = ?presence,
            "notification hook: notification suppressed"
        );
        return StatusCode::OK;
    }

    let notifier = Arc::clone(&state.notifier);
    let title = "Claude Code (waiting)".to_string();
    let msg_body = payload.message.as_deref().unwrap_or("Permission prompt");
    let message = format!("[{project}] {msg_body}");
    let delay_secs = global.notification_delay_secs;

    if delay_secs == 0 {
        tokio::spawn(async move {
            fire_and_forget(&*notifier, &title, &message, None).await;
        });
    } else {
        let cancel = state.pending.insert(&session_id).await;
        let pending = Arc::clone(&state.pending);
        let sid = session_id.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(Duration::from_secs(delay_secs)) => {
                    fire_and_forget(&*notifier, &title, &message, None).await;
                    pending.remove(&sid).await;
                }
            }
        });
    }

    StatusCode::OK
}

/// POST /hooks/session-end
pub async fn session_end<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    if let Some(session_id) = &payload.session_id {
        state.pending.cancel(session_id).await;
        state.sessions.deregister(session_id).await;
    }
    StatusCode::OK
}

fn extract_session(payload: &HookPayload) -> Option<(String, String)> {
    match (&payload.session_id, &payload.cwd) {
        (Some(sid), Some(cwd)) => Some((sid.clone(), cwd.clone())),
        _ => None,
    }
}

/// Request body for POST /hooks/approval
#[derive(Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub session_id: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub context: Option<String>,
}

/// Response for POST /hooks/approval
#[derive(Serialize)]
pub struct ApprovalResponse {
    pub id: uuid::Uuid,
    #[serde(flatten)]
    pub status: ApprovalStatus,
}

/// POST /api/v1/hooks/approval — register a pending approval request.
pub async fn approval<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(req): Json<ApprovalRequest>,
) -> Json<ApprovalResponse> {
    let project = state
        .sessions
        .get_or_register(&req.session_id, &req.cwd)
        .await;

    let approval = state
        .approvals
        .register(
            req.request_id,
            req.session_id,
            project.clone(),
            req.tool_name.clone(),
            req.tool_input.clone(),
            req.context.clone(),
        )
        .await;

    // Send push notification with link if pending and base_url configured
    if !approval.status.is_resolved() {
        if let Some(base_url) = &state.config.base_url {
            let url = format!("{}/approvals/{}", base_url, approval.id);
            let notifier = Arc::clone(&state.notifier);
            let title = "Claude Code (approval)".to_string();
            let message = format!("[{project}] {} — {}", req.tool_name, truncate_input(&req.tool_input));
            tokio::spawn(async move {
                fire_and_forget(&*notifier, &title, &message, Some(&url)).await;
            });
        }
    }

    Json(ApprovalResponse {
        id: approval.id,
        status: approval.status,
    })
}

fn truncate_input(input: &serde_json::Value) -> String {
    let s = input.to_string();
    if s.len() > 100 {
        format!("{}...", &s[..100])
    } else {
        s
    }
}

async fn fire_and_forget<N: Notifier>(notifier: &N, title: &str, message: &str, url: Option<&str>) {
    if let Err(e) = notifier.send(title, message, url).await {
        warn!("{} notification failed: {e}", notifier.name());
    }
}

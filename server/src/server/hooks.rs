use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use capabilities::ApprovalContext;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::AppState;
use super::approvals;
use super::approvals::ApprovalStatus;
use super::notifier::Notifier;
use super::sessions::{EditorType, SessionStatus};
use crate::server::presence::PresenceState;

/// Common fields from hook payloads. Serde ignores unknown fields by default.
#[derive(Deserialize)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub message: Option<String>,
    pub editor_type: Option<EditorType>,
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

    let project = state
        .sessions
        .get_or_register(&session_id, &cwd, payload.editor_type)
        .await;
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

    state
        .sessions
        .set_status(&session_id, SessionStatus::Ended, None, None)
        .await;

    StatusCode::OK
}

/// POST /hooks/session-end
pub async fn session_end<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    if let Some(session_id) = &payload.session_id {
        state.pending.cancel(session_id).await;
        state
            .sessions
            .set_status(session_id, SessionStatus::Ended, None, None)
            .await;
    }
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

    let project = state
        .sessions
        .get_or_register(&session_id, &cwd, payload.editor_type)
        .await;
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

fn extract_session(payload: &HookPayload) -> Option<(String, String)> {
    match (&payload.session_id, &payload.cwd) {
        (Some(sid), Some(cwd)) => Some((sid.clone(), cwd.clone())),
        _ => None,
    }
}

/// Request body for POST /api/v1/hooks/approval
#[derive(Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub session_id: String,
    pub session_display_name: String,
    pub cwd: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    /// Provider identifier: "claude-code" | "cursor" | "opencode"
    pub provider: String,
    /// Request type: "tool_use" (Phase 2 will add "plan_question")
    pub request_type: String,
    pub context: ApprovalContext,
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
        .get_or_register(&req.session_id, &req.cwd, None)
        .await;

    // Cache the display name on the session (it arrives with every approval request)
    if !req.session_display_name.is_empty() {
        state
            .sessions
            .set_display_name(&req.session_id, req.session_display_name.clone())
            .await;
    }

    let approval = state
        .approvals
        .register(approvals::RegisterApproval {
            request_id: req.id,
            session_id: req.session_id,
            session_display_name: req.session_display_name,
            project: project.clone(),
            tool_name: req.tool_name.clone(),
            tool_input: req.tool_input.clone(),
            provider: req.provider,
            request_type: req.request_type,
            context: req.context,
        })
        .await;

    // Send push notification with link if pending and base_url configured
    if !approval.status.is_resolved()
        && let Some(base_url) = &state.config.base_url
    {
        let url = format!("{}/approvals/{}", base_url, approval.id);
        let notifier = Arc::clone(&state.notifier);
        let title = "Agent Hub (approval)".to_string();
        let message = format!(
            "[{project}] {} — {}",
            req.tool_name,
            truncate_input(&req.tool_input)
        );
        tokio::spawn(async move {
            fire_and_forget(&*notifier, &title, &message, Some(&url)).await;
        });
    }

    Json(ApprovalResponse {
        id: approval.id,
        status: approval.status,
    })
}

fn truncate_input(input: &serde_json::Value) -> String {
    let s = input.to_string();
    if s.len() > 100 {
        let end = s.char_indices().nth(100).map_or(s.len(), |(i, _)| i);
        format!("{}...", &s[..end])
    } else {
        s
    }
}

/// Request body for POST /api/v1/hooks/status
#[derive(Deserialize)]
pub struct StatusReport {
    pub session_id: String,
    pub cwd: String,
    pub status: SessionStatus,
    pub waiting_reason: Option<String>,
    pub display_name: Option<String>,
    pub editor_type: Option<EditorType>,
}

/// POST /api/v1/hooks/status — report session status from a client.
pub async fn status<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(req): Json<StatusReport>,
) -> StatusCode {
    // Ensure session exists
    state
        .sessions
        .get_or_register(&req.session_id, &req.cwd, req.editor_type)
        .await;

    state
        .sessions
        .set_status(
            &req.session_id,
            req.status,
            req.waiting_reason,
            req.display_name,
        )
        .await;

    // Cancel pending notifications when session ends
    if req.status == SessionStatus::Ended {
        state.pending.cancel(&req.session_id).await;
    }

    StatusCode::OK
}

async fn fire_and_forget<N: Notifier>(notifier: &N, title: &str, message: &str, url: Option<&str>) {
    if let Err(e) = notifier.send(title, message, url).await {
        warn!("{} notification failed: {e}", notifier.name());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the exact JSON the opencode plugin sends deserializes correctly.
    /// This is the payload from spawnStatusReport() in agent-hub.ts.
    #[test]
    fn deserialize_status_report_from_opencode_plugin() {
        let json = r#"{
            "session_id": "ses_abc123",
            "cwd": "/home/nick/workspaces/myapp",
            "status": "idle",
            "waiting_reason": null,
            "display_name": "Fix auth bug",
            "editor_type": "opencode"
        }"#;
        let report: StatusReport =
            serde_json::from_str(json).expect("opencode status report should deserialize");
        assert_eq!(report.session_id, "ses_abc123");
        assert_eq!(report.status, SessionStatus::Idle);
        assert_eq!(
            report.editor_type,
            Some(super::super::sessions::EditorType::Opencode)
        );
    }

    /// Verify that a status report with editor_type omitted still works
    /// (editor_type is Option<EditorType>).
    #[test]
    fn deserialize_status_report_without_editor_type() {
        let json = r#"{
            "session_id": "ses_abc123",
            "cwd": "/home/nick/workspaces/myapp",
            "status": "active"
        }"#;
        let report: StatusReport =
            serde_json::from_str(json).expect("minimal status report should deserialize");
        assert_eq!(report.status, SessionStatus::Active);
        assert!(report.editor_type.is_none());
    }

    /// Every status variant the plugin can send should deserialize.
    #[test]
    fn deserialize_status_report_all_statuses() {
        for status in ["active", "idle", "waiting", "ended"] {
            let json = format!(
                r#"{{"session_id":"s1","cwd":"/tmp","status":"{}","editor_type":"opencode"}}"#,
                status
            );
            serde_json::from_str::<StatusReport>(&json)
                .unwrap_or_else(|e| panic!("status={status} should work: {e}"));
        }
    }
}

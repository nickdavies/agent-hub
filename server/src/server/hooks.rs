use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

// Re-export protocol types used by this module.
use protocol::SessionStatus;
pub use protocol::{
    ApprovalRequest, ApprovalResponse, NotificationPayload, QuestionProxyRequest,
    QuestionProxyResponse, SessionEndPayload, SessionId, StatusReport, StopPayload,
};

use super::AppState;
use super::approvals;
use super::notifier::Notifier;
use super::questions;
use crate::server::presence::PresenceState;

/// Tracks pending delayed notifications so they can be cancelled.
pub struct PendingNotifications {
    pending: RwLock<HashMap<SessionId, CancellationToken>>,
}

impl PendingNotifications {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Register a cancellation token for a session. Cancels any existing pending notification.
    async fn insert(&self, session_id: &SessionId) -> CancellationToken {
        let mut map = self.pending.write().await;
        // Cancel any existing pending notification for this session
        if let Some(old) = map.remove(session_id) {
            old.cancel();
        }
        let token = CancellationToken::new();
        map.insert(session_id.clone(), token.clone());
        token
    }

    /// Cancel and remove a pending notification for a session.
    pub async fn cancel(&self, session_id: &SessionId) {
        let mut map = self.pending.write().await;
        if let Some(token) = map.remove(session_id) {
            token.cancel();
            info!(session_id = %session_id, "pending notification cancelled");
        }
    }

    /// Remove the token (called when the delayed notification fires successfully).
    async fn remove(&self, session_id: &SessionId) {
        self.pending.write().await.remove(session_id);
    }
}

/// POST /hooks/stop
pub async fn stop<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<StopPayload>,
) -> StatusCode {
    let session_id = &payload.session_id;

    // Cancel any pending permission notification before sending the stop notification
    state.pending.cancel(session_id).await;

    let project = state
        .sessions
        .get_or_register(session_id, &payload.cwd, payload.editor_type)
        .await;
    let session_cfg = state.sessions.get_config(session_id).await;
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
            session_id = %session_id,
            present = ?presence,
            "stop hook: notification suppressed"
        );
    }

    state
        .sessions
        .set_status(session_id, SessionStatus::Ended, None, None)
        .await;

    StatusCode::OK
}

/// POST /hooks/session-end
pub async fn session_end<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<SessionEndPayload>,
) -> StatusCode {
    state.pending.cancel(&payload.session_id).await;
    state
        .sessions
        .set_status(&payload.session_id, SessionStatus::Ended, None, None)
        .await;
    StatusCode::OK
}

/// POST /hooks/notification
pub async fn notification<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(payload): Json<NotificationPayload>,
) -> StatusCode {
    let session_id = &payload.session_id;

    let project = state
        .sessions
        .get_or_register(session_id, &payload.cwd, payload.editor_type)
        .await;
    let session_cfg = state.sessions.get_config(session_id).await;
    let presence = state.presence.get().await;
    let global = state.notify_config.read().await;

    let should_notify = presence != PresenceState::Present
        && global.permission_enabled
        && session_cfg.as_ref().is_some_and(|c| c.permission_enabled);

    if !should_notify {
        info!(
            session_id = %session_id,
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
        let cancel = state.pending.insert(session_id).await;
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

    let outcome = state
        .approvals
        .register_with_outcome(approvals::RegisterApproval {
            request_id: req.id,
            session_id: req.session_id,
            session_display_name: req.session_display_name,
            project: project.clone(),
            tool: req.tool.clone(),
            tool_input: req.tool_input.clone(),
            provider: req.provider,
            request_type: req.request_type,
            context: req.context,
        })
        .await;
    let approval = outcome.approval;

    // Send push notification with link if pending and base_url configured
    if outcome.is_new
        && !approval.status.is_resolved()
        && let Some(base_url) = &state.config.base_url
    {
        let url = format!("{}/approvals/{}", base_url, approval.id);
        let notifier = Arc::clone(&state.notifier);
        let title = "Agent Hub (approval)".to_string();
        let message = format!(
            "[{}] {} — {}",
            approval.project,
            approval.tool,
            truncate_input(&approval.tool_input)
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

/// POST /api/v1/hooks/question — register a proxied question from the gateway.
pub async fn question<N: Notifier>(
    State(state): State<AppState<N>>,
    Json(req): Json<QuestionProxyRequest>,
) -> Json<QuestionProxyResponse> {
    let project = state
        .sessions
        .get_or_register(&req.session_id, &req.cwd, None)
        .await;

    if !req.session_display_name.is_empty() {
        state
            .sessions
            .set_display_name(&req.session_id, req.session_display_name.clone())
            .await;
    }

    let pq = state
        .questions
        .register(questions::RegisterQuestion {
            request_id: req.id,
            session_id: req.session_id,
            session_display_name: req.session_display_name,
            project: project.clone(),
            question_request_id: req.question_request_id,
            questions: req.questions,
            provider: req.provider,
        })
        .await;

    Json(QuestionProxyResponse {
        id: pq.id,
        status: pq.status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Provider;

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
        assert_eq!(report.editor_type, Some(Provider::Opencode));
    }

    /// Verify that a status report with editor_type omitted still works
    /// (editor_type is Option<Provider>).
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

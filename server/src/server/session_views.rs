use super::AppState;
use super::approvals;
use super::notifier::Notifier;
use super::questions;
use super::sessions::{self, EffectiveSessionStatus, SessionStatus};

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
pub(crate) async fn build_session_views<N: Notifier>(
    state: &AppState<N>,
) -> Vec<sessions::SessionView> {
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

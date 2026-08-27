use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, RwLock, watch};
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

// Re-export protocol types so existing `use super::approvals::X` imports work.
pub use protocol::{Approval, ApprovalContext, ApprovalStatus};

use protocol::{ApprovalGrantDuration, SessionId, Tool};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ApprovalGrantKey {
    session_id: SessionId,
    command: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TemporaryGrantError {
    #[error("approval not found")]
    ApprovalNotFound,
    #[error("temporary grants require a pending approval decision")]
    AlreadyResolved,
    #[error("temporary grants require an approve decision")]
    NotApproved,
    #[error("temporary grants are supported only for Bash approvals")]
    NotBash,
    #[error("Bash approval does not contain a string command")]
    MissingCommand,
}

struct ApprovalEntry {
    approval: Approval,
    tx: watch::Sender<ApprovalStatus>,
    /// Tracks when a gateway last polled `/wait` for this approval.
    /// `None` means no poll has occurred yet (freshly registered).
    last_polled_at: Option<Instant>,
}

pub struct ApprovalRegistry {
    registration: Mutex<()>,
    entries: RwLock<HashMap<Uuid, ApprovalEntry>>,
    /// request_id -> approval Uuid (idempotency)
    by_request_id: RwLock<HashMap<String, Uuid>>,
    /// session_id -> approval Uuids (multiple approvals per session)
    by_session_id: RwLock<HashMap<SessionId, HashSet<Uuid>>>,
    grants: RwLock<HashMap<ApprovalGrantKey, Instant>>,
}

pub struct RegisterApproval {
    pub request_id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub project: String,
    pub tool: protocol::Tool,
    pub tool_input: serde_json::Value,
    pub provider: String,
    pub request_type: protocol::RequestType,
    pub context: ApprovalContext,
}

pub(crate) struct RegistrationOutcome {
    pub approval: Approval,
    pub is_new: bool,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            registration: Mutex::new(()),
            entries: RwLock::new(HashMap::new()),
            by_request_id: RwLock::new(HashMap::new()),
            by_session_id: RwLock::new(HashMap::new()),
            grants: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new approval or return existing one if request_id matches.
    #[cfg(test)]
    pub async fn register(&self, params: RegisterApproval) -> Approval {
        self.register_with_outcome(params).await.approval
    }

    pub(crate) async fn register_with_outcome(
        &self,
        params: RegisterApproval,
    ) -> RegistrationOutcome {
        self.register_at_with_outcome(params, Utc::now(), Instant::now())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn register_at(
        &self,
        params: RegisterApproval,
        created_at: DateTime<Utc>,
        now: Instant,
    ) -> Approval {
        self.register_at_with_outcome(params, created_at, now)
            .await
            .approval
    }

    async fn register_at_with_outcome(
        &self,
        params: RegisterApproval,
        created_at: DateTime<Utc>,
        now: Instant,
    ) -> RegistrationOutcome {
        let _registration = self.registration.lock().await;
        // Idempotency: if request_id already registered, return existing
        let existing = {
            let by_req = self.by_request_id.read().await;
            if let Some(&existing_id) = by_req.get(&params.request_id) {
                let entries = self.entries.read().await;
                entries
                    .get(&existing_id)
                    .map(|entry| entry.approval.clone())
            } else {
                None
            }
        };
        if let Some(existing) = existing {
            let approval = if !existing.status.is_resolved()
                && self.has_matching_grant(&existing, now).await
            {
                self.resolve(existing.id, ApprovalStatus::Approved { message: None })
                    .await
                    .expect("registered approval must exist")
            } else {
                existing
            };
            return RegistrationOutcome {
                approval,
                is_new: false,
            };
        }

        let grant_key = grant_key(&params.session_id, &params.tool, &params.tool_input);
        let granted = if let Some(key) = grant_key.as_ref() {
            self.has_grant(key, now).await
        } else {
            false
        };

        let id = Uuid::new_v4();
        let approval = Approval {
            id,
            request_id: params.request_id.clone(),
            session_id: params.session_id.clone(),
            session_display_name: params.session_display_name,
            project: params.project,
            tool: params.tool,
            tool_input: params.tool_input,
            provider: params.provider,
            request_type: params.request_type,
            context: params.context,
            created_at,
            status: if granted {
                ApprovalStatus::Approved { message: None }
            } else {
                ApprovalStatus::Pending
            },
        };

        let (tx, _rx) = watch::channel(approval.status.clone());
        let entry = ApprovalEntry {
            approval: approval.clone(),
            tx,
            last_polled_at: None,
        };

        let mut by_sess = self.by_session_id.write().await;
        let mut entries = self.entries.write().await;
        let mut by_req = self.by_request_id.write().await;
        entries.insert(id, entry);
        by_req.insert(params.request_id, id);
        by_sess.entry(params.session_id).or_default().insert(id);

        info!(approval_id = %id, "approval registered");
        RegistrationOutcome {
            approval,
            is_new: true,
        }
    }

    async fn has_matching_grant(&self, approval: &Approval, now: Instant) -> bool {
        let Some(key) = grant_key(&approval.session_id, &approval.tool, &approval.tool_input)
        else {
            return false;
        };
        self.has_grant(&key, now).await
    }

    async fn has_grant(&self, key: &ApprovalGrantKey, now: Instant) -> bool {
        let mut grants = self.grants.write().await;
        grants.retain(|_, expires_at| *expires_at > now);
        grants.contains_key(key)
    }

    /// Get an approval by id.
    pub async fn get(&self, id: Uuid) -> Option<Approval> {
        let entries = self.entries.read().await;
        entries.get(&id).map(|e| e.approval.clone())
    }

    /// Subscribe to status changes for an approval.
    /// Returns current status and a receiver for future changes.
    pub async fn subscribe(&self, id: Uuid) -> Option<watch::Receiver<ApprovalStatus>> {
        let entries = self.entries.read().await;
        entries.get(&id).map(|e| e.tx.subscribe())
    }

    /// Resolve an approval (approve/deny/cancel).
    pub async fn resolve(&self, id: Uuid, status: ApprovalStatus) -> Option<Approval> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(&id)?;
        if entry.approval.status.is_resolved() {
            // Already resolved, return current state
            return Some(entry.approval.clone());
        }
        entry.approval.status = status.clone();
        // Notify all watchers
        let _ = entry.tx.send(status);
        Some(entry.approval.clone())
    }

    pub async fn resolve_with_grant(
        &self,
        id: Uuid,
        status: ApprovalStatus,
        duration: ApprovalGrantDuration,
    ) -> Result<Approval, TemporaryGrantError> {
        self.resolve_with_grant_at(id, status, duration, Instant::now())
            .await
    }

    pub(crate) async fn resolve_with_grant_at(
        &self,
        id: Uuid,
        status: ApprovalStatus,
        duration: ApprovalGrantDuration,
        now: Instant,
    ) -> Result<Approval, TemporaryGrantError> {
        if !matches!(status, ApprovalStatus::Approved { .. }) {
            return Err(TemporaryGrantError::NotApproved);
        }

        let _registration = self.registration.lock().await;
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(&id)
            .ok_or(TemporaryGrantError::ApprovalNotFound)?;
        if entry.approval.status.is_resolved() {
            return Err(TemporaryGrantError::AlreadyResolved);
        }
        if entry.approval.tool != Tool::Bash {
            return Err(TemporaryGrantError::NotBash);
        }
        let key = grant_key(
            &entry.approval.session_id,
            &entry.approval.tool,
            &entry.approval.tool_input,
        )
        .ok_or(TemporaryGrantError::MissingCommand)?;

        self.grants
            .write()
            .await
            .insert(key, now + duration.duration());
        entry.approval.status = status.clone();
        let _ = entry.tx.send(status);
        Ok(entry.approval.clone())
    }

    pub async fn purge_expired_grants(&self) -> usize {
        self.purge_expired_grants_at(Instant::now()).await
    }

    pub(crate) async fn purge_expired_grants_at(&self, now: Instant) -> usize {
        let mut grants = self.grants.write().await;
        let previous_len = grants.len();
        grants.retain(|_, expires_at| *expires_at > now);
        previous_len - grants.len()
    }

    /// List only pending approvals, sorted by creation time (oldest first).
    pub async fn list_pending(&self) -> Vec<Approval> {
        let entries = self.entries.read().await;
        let mut pending: Vec<Approval> = entries
            .values()
            .filter(|e| !e.approval.status.is_resolved())
            .map(|e| e.approval.clone())
            .collect();
        pending.sort_by_key(|a| a.created_at);
        pending
    }

    /// Returns the first (oldest) pending approval for the given session, if any.
    pub async fn first_pending_for_session(&self, session_id: &SessionId) -> Option<Approval> {
        let by_session = self.by_session_id.read().await;
        let entries = self.entries.read().await;
        by_session.get(session_id).and_then(|ids| {
            ids.iter()
                .filter_map(|id| entries.get(id))
                .filter(|e| !e.approval.status.is_resolved())
                .min_by_key(|e| e.approval.created_at)
                .map(|e| e.approval.clone())
        })
    }

    /// Remove all approvals for a session (on session eviction).
    pub async fn evict_session(&self, session_id: &SessionId) {
        let _registration = self.registration.lock().await;
        let approval_ids = {
            let mut by_sess = self.by_session_id.write().await;
            by_sess.remove(session_id).unwrap_or_default()
        };

        for id in approval_ids {
            self.resolve(id, ApprovalStatus::Cancelled).await;
            let request_id = {
                let mut entries = self.entries.write().await;
                entries.remove(&id).map(|e| e.approval.request_id)
            };
            if let Some(req_id) = request_id {
                let mut by_req = self.by_request_id.write().await;
                by_req.remove(&req_id);
            }
            info!(approval_id = %id, session_id = %session_id, "approval evicted with session");
        }
    }

    /// Record that a gateway polled for this approval (called from the wait handler).
    pub async fn touch(&self, id: Uuid) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&id) {
            entry.last_polled_at = Some(Instant::now());
        }
    }

    /// Cancel pending approvals whose gateway has stopped polling.
    ///
    /// An approval is considered orphaned when:
    /// - It is still pending, AND
    /// - A gateway has polled for it at least once (`last_polled_at` is set), AND
    /// - The last poll was longer ago than `threshold`.
    ///
    /// Returns the number of approvals cancelled.
    pub async fn evict_orphaned(&self, threshold: Duration) -> usize {
        let now = Instant::now();
        let orphaned_ids: Vec<Uuid> = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .filter(|(_, entry)| {
                    !entry.approval.status.is_resolved()
                        && entry
                            .last_polled_at
                            .is_some_and(|t| now.duration_since(t) > threshold)
                })
                .map(|(&id, _)| id)
                .collect()
        };

        let count = orphaned_ids.len();
        for id in orphaned_ids {
            self.resolve(id, ApprovalStatus::Cancelled).await;
            info!(approval_id = %id, "approval cancelled (orphaned — gateway stopped polling)");
        }
        count
    }

    /// Export pending approvals for persistence, sorted by creation time.
    pub async fn snapshot(&self) -> Vec<Approval> {
        let entries = self.entries.read().await;
        let mut pending: Vec<Approval> = entries
            .values()
            .filter(|e| !e.approval.status.is_resolved())
            .map(|e| e.approval.clone())
            .collect();
        pending.sort_by_key(|a| a.created_at);
        pending
    }

    /// Restore approvals from persisted state.
    pub async fn restore(&self, approvals: Vec<Approval>) {
        let _registration = self.registration.lock().await;

        for approval in approvals {
            if approval.status.is_resolved() {
                continue;
            }
            let id = approval.id;
            let request_id = approval.request_id.clone();
            let session_id = approval.session_id.clone();

            let (tx, _rx) = watch::channel(ApprovalStatus::Pending);
            self.entries.write().await.insert(
                id,
                ApprovalEntry {
                    approval,
                    tx,
                    last_polled_at: None,
                },
            );
            self.by_request_id.write().await.insert(request_id, id);
            self.by_session_id
                .write()
                .await
                .entry(session_id)
                .or_default()
                .insert(id);
            info!(approval_id = %id, "approval restored");
        }
    }
}

fn grant_key(
    session_id: &SessionId,
    tool: &Tool,
    tool_input: &serde_json::Value,
) -> Option<ApprovalGrantKey> {
    if tool != &Tool::Bash {
        return None;
    }
    let command = tool_input.get("command")?.as_str()?;
    Some(ApprovalGrantKey {
        session_id: session_id.clone(),
        command: command.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use protocol::{ApprovalGrantDuration, HookEventName, RequestType, Tool};
    use serde_json::json;
    use tokio::time::Instant;

    use super::{ApprovalContext, ApprovalRegistry, ApprovalStatus, RegisterApproval};

    fn request(request_id: &str, session_id: &str, tool: Tool, command: &str) -> RegisterApproval {
        RegisterApproval {
            request_id: request_id.to_string(),
            session_id: protocol::SessionId::new(session_id),
            session_display_name: "test session".to_string(),
            project: "/workspace".to_string(),
            tool,
            tool_input: json!({"command": command}),
            provider: "opencode".to_string(),
            request_type: RequestType::ToolUse,
            context: ApprovalContext {
                workspace_roots: vec!["/workspace".to_string()],
                hook_event_name: HookEventName::Other("test".to_string()),
                extra: None,
            },
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap()
    }

    async fn poll_until_blocked<F: Future>(mut future: std::pin::Pin<&mut F>) {
        poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("registration unexpectedly completed before the barrier"),
        })
        .await;
    }

    #[tokio::test]
    async fn concurrent_duplicate_request_registration_keeps_the_first_writer() {
        let registry = ApprovalRegistry::new();
        let grant_lock_barrier = registry.grants.write().await;
        let first_params = request(
            "duplicate-request",
            "first-session",
            Tool::Bash,
            "first command",
        );
        let second_params = request(
            "duplicate-request",
            "second-session",
            Tool::Bash,
            "second command",
        );
        let monotonic_now = Instant::now();
        let mut first = Box::pin(registry.register_at(first_params, now(), monotonic_now));
        let mut second = Box::pin(registry.register_at(second_params, now(), monotonic_now));

        // The first registration stops at the grant barrier while the second
        // waits for the registration lock.
        poll_until_blocked(first.as_mut()).await;
        poll_until_blocked(second.as_mut()).await;
        drop(grant_lock_barrier);

        let (first_result, second_result) = tokio::join!(first, second);
        let entries = registry.entries.read().await;
        let request_ids = registry.by_request_id.read().await;
        let sessions = registry.by_session_id.read().await;

        assert_eq!(first_result.id, second_result.id);
        assert_eq!(entries.len(), 1);
        assert_eq!(request_ids.len(), 1);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            first_result.session_id,
            protocol::SessionId::new("first-session")
        );
        assert_eq!(first_result.tool_input, json!({"command": "first command"}));
        assert_eq!(second_result.session_id, first_result.session_id);
        assert_eq!(second_result.tool_input, first_result.tool_input);
    }

    #[tokio::test]
    async fn matching_registration_after_grant_is_approved() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();

        let registered_before_grant = registry
            .register_at(
                request("grant-source", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        let resolved = registry
            .resolve_with_grant_at(
                registered_before_grant.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .expect("pending Bash approval should create a grant");

        let registered_after_grant = registry
            .register_at(
                request("grant-match", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;

        assert_eq!(resolved.status, ApprovalStatus::Approved { message: None });
        assert_eq!(
            registered_after_grant.status,
            ApprovalStatus::Approved { message: None }
        );
    }

    #[tokio::test]
    async fn matching_registration_before_grant_remains_pending() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let grant_source = registry
            .register_at(
                request("grant-source", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        let registered_before_grant = registry
            .register_at(
                request("grant-match", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;

        registry
            .resolve_with_grant_at(
                grant_source.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .expect("pending Bash approval should create a grant");

        assert_eq!(registered_before_grant.status, ApprovalStatus::Pending);
        assert_eq!(
            registry
                .get(registered_before_grant.id)
                .await
                .expect("matching registration should be stored")
                .status,
            ApprovalStatus::Pending
        );
    }

    #[tokio::test]
    async fn matching_registration_cannot_publish_pending_after_grant_becomes_active() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let grant_source = registry
            .register_at(
                request("grant-source", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        let entries_barrier = registry.entries.write().await;
        let mut create_grant = Box::pin(registry.resolve_with_grant_at(
            grant_source.id,
            ApprovalStatus::Approved { message: None },
            ApprovalGrantDuration::ThirtyMinutes,
            monotonic_now,
        ));
        let mut matching_registration = Box::pin(registry.register_at(
            request("grant-match", "session-1", Tool::Bash, "cargo test"),
            now(),
            monotonic_now,
        ));

        // The grant creator reaches the entries barrier first. Registration
        // must remain behind it for the complete registration → entries →
        // grants critical section.
        poll_until_blocked(create_grant.as_mut()).await;
        poll_until_blocked(matching_registration.as_mut()).await;
        drop(entries_barrier);

        let (grant_result, registration_result) = tokio::join!(create_grant, matching_registration);
        grant_result.expect("pending Bash approval should create a grant");

        assert_eq!(
            registration_result.status,
            ApprovalStatus::Approved { message: None },
            "a registration published after its matching grant became active must not be pending"
        );
        assert_eq!(
            registry
                .get(registration_result.id)
                .await
                .expect("matching registration should be stored")
                .status,
            ApprovalStatus::Approved { message: None }
        );
    }

    #[tokio::test]
    async fn temporary_grant_uses_monotonic_expiry_before_at_and_after_boundary() {
        let registry = ApprovalRegistry::new();
        let wall_created_at = now();
        let monotonic_start = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                wall_created_at,
                monotonic_start,
            )
            .await;
        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_start,
            )
            .await
            .unwrap();
        let expires_at = monotonic_start + std::time::Duration::from_secs(30 * 60);

        let before = registry
            .register_at(
                request("request-before", "session-1", Tool::Bash, "cargo test"),
                wall_created_at + ChronoDuration::days(365),
                expires_at - std::time::Duration::from_nanos(1),
            )
            .await;
        let at = registry
            .register_at(
                request("request-at", "session-1", Tool::Bash, "cargo test"),
                wall_created_at - ChronoDuration::days(365),
                expires_at,
            )
            .await;
        let after = registry
            .register_at(
                request("request-after", "session-1", Tool::Bash, "cargo test"),
                wall_created_at,
                expires_at + std::time::Duration::from_nanos(1),
            )
            .await;

        assert_eq!(
            before.created_at,
            wall_created_at + ChronoDuration::days(365)
        );
        assert!(
            before.status.is_resolved(),
            "grant should match before expiry"
        );
        assert_eq!(at.status, ApprovalStatus::Pending);
        assert_eq!(after.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn temporary_grants_are_not_restored_from_a_snapshot() {
        let source = ApprovalRegistry::new();
        let wall_created_at = now();
        let monotonic_start = Instant::now();
        let granted = source
            .register_at(
                request("grant-source", "session-1", Tool::Bash, "cargo test"),
                wall_created_at,
                monotonic_start,
            )
            .await;
        source
            .resolve_with_grant_at(
                granted.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::TwentyFourHours,
                monotonic_start,
            )
            .await
            .unwrap();
        source
            .register_at(
                request("persisted-pending", "session-2", Tool::Write, "ignored"),
                wall_created_at,
                monotonic_start,
            )
            .await;

        let restored = ApprovalRegistry::new();
        restored.restore(source.snapshot().await).await;
        let matching = restored
            .register_at(
                request("after-restore", "session-1", Tool::Bash, "cargo test"),
                wall_created_at + ChronoDuration::seconds(1),
                monotonic_start + std::time::Duration::from_secs(1),
            )
            .await;

        assert_eq!(matching.status, ApprovalStatus::Pending);
        assert_eq!(restored.list_pending().await.len(), 2);
    }

    #[tokio::test]
    async fn one_time_approval_does_not_create_a_grant() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        registry
            .resolve(first.id, ApprovalStatus::Approved { message: None })
            .await;

        let next = registry
            .register_at(
                request("request-2", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::seconds(1),
                monotonic_now + std::time::Duration::from_secs(1),
            )
            .await;

        assert_eq!(next.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn grant_matches_only_the_exact_session_and_bash_command() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .expect("Bash approval should support a temporary grant");

        let matching = registry
            .register_at(
                request("request-2", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::seconds(1),
                monotonic_now + std::time::Duration::from_secs(1),
            )
            .await;
        let other_session = registry
            .register_at(
                request("request-3", "session-2", Tool::Bash, "cargo test"),
                now() + ChronoDuration::seconds(1),
                monotonic_now + std::time::Duration::from_secs(1),
            )
            .await;
        let other_command = registry
            .register_at(
                request("request-4", "session-1", Tool::Bash, "cargo test --all"),
                now() + ChronoDuration::seconds(1),
                monotonic_now + std::time::Duration::from_secs(1),
            )
            .await;

        assert_eq!(matching.status, ApprovalStatus::Approved { message: None });
        assert_eq!(other_session.status, ApprovalStatus::Pending);
        assert_eq!(other_command.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn grant_command_matching_is_case_and_whitespace_sensitive() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .unwrap();

        for (request_id, command) in [
            ("request-case", "Cargo test"),
            ("request-leading-space", " cargo test"),
            ("request-trailing-space", "cargo test "),
            ("request-inner-space", "cargo  test"),
        ] {
            let approval = registry
                .register_at(
                    request(request_id, "session-1", Tool::Bash, command),
                    now() + ChronoDuration::seconds(1),
                    monotonic_now + std::time::Duration::from_secs(1),
                )
                .await;
            assert_eq!(
                approval.status,
                ApprovalStatus::Pending,
                "command variant {command:?} must not match the grant"
            );
        }
    }

    #[tokio::test]
    async fn newer_grant_replaces_the_expiry_for_the_same_session_and_command() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        let second = registry
            .register_at(
                request("request-2", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;

        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::TwentyFourHours,
                monotonic_now,
            )
            .await
            .unwrap();
        let replacement_time = monotonic_now + std::time::Duration::from_secs(60 * 60);
        registry
            .resolve_with_grant_at(
                second.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                replacement_time,
            )
            .await
            .unwrap();

        let at_replacement_expiry = registry
            .register_at(
                request("request-3", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::minutes(90),
                replacement_time + std::time::Duration::from_secs(30 * 60),
            )
            .await;

        assert_eq!(at_replacement_expiry.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn grant_is_active_before_but_not_at_or_after_expiry() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .unwrap();
        let expires_at = monotonic_now + std::time::Duration::from_secs(30 * 60);

        let before = registry
            .register_at(
                request("request-before", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::minutes(30),
                expires_at - std::time::Duration::from_nanos(1),
            )
            .await;
        let at = registry
            .register_at(
                request("request-at", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::minutes(30),
                expires_at,
            )
            .await;
        let after = registry
            .register_at(
                request("request-after", "session-1", Tool::Bash, "cargo test"),
                now() + ChronoDuration::minutes(30),
                expires_at + std::time::Duration::from_nanos(1),
            )
            .await;

        assert!(
            before.status.is_resolved(),
            "grant should match before expiry"
        );
        assert_eq!(at.status, ApprovalStatus::Pending);
        assert_eq!(after.status, ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn purge_removes_expired_grants() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let first = registry
            .register_at(
                request("request-1", "session-1", Tool::Bash, "cargo test"),
                now(),
                monotonic_now,
            )
            .await;
        registry
            .resolve_with_grant_at(
                first.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::ThirtyMinutes,
                monotonic_now,
            )
            .await
            .unwrap();

        assert_eq!(
            registry
                .purge_expired_grants_at(monotonic_now + std::time::Duration::from_secs(30 * 60),)
                .await,
            1
        );
        assert_eq!(
            registry
                .purge_expired_grants_at(monotonic_now + std::time::Duration::from_secs(60 * 60),)
                .await,
            0
        );
    }

    #[tokio::test]
    async fn temporary_grant_rejects_non_bash_approvals() {
        let registry = ApprovalRegistry::new();
        let monotonic_now = Instant::now();
        let approval = registry
            .register_at(
                request("request-1", "session-1", Tool::Write, "ignored"),
                now(),
                monotonic_now,
            )
            .await;

        registry
            .resolve_with_grant_at(
                approval.id,
                ApprovalStatus::Approved { message: None },
                ApprovalGrantDuration::OneHour,
                monotonic_now,
            )
            .await
            .expect_err("temporary grants must be Bash-only");

        assert_eq!(
            registry.get(approval.id).await.unwrap().status,
            ApprovalStatus::Pending
        );
    }
}

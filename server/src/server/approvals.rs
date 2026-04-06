use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{RwLock, watch};
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

// Re-export protocol types so existing `use super::approvals::X` imports work.
pub use protocol::{Approval, ApprovalContext, ApprovalStatus};

struct ApprovalEntry {
    approval: Approval,
    tx: watch::Sender<ApprovalStatus>,
    /// Tracks when a gateway last polled `/wait` for this approval.
    /// `None` means no poll has occurred yet (freshly registered).
    last_polled_at: Option<Instant>,
}

pub struct ApprovalRegistry {
    entries: RwLock<HashMap<Uuid, ApprovalEntry>>,
    /// request_id -> approval Uuid (idempotency)
    by_request_id: RwLock<HashMap<String, Uuid>>,
    /// session_id -> approval Uuids (multiple approvals per session)
    by_session_id: RwLock<HashMap<String, HashSet<Uuid>>>,
}

pub struct RegisterApproval {
    pub request_id: String,
    pub session_id: String,
    pub session_display_name: String,
    pub project: String,
    pub tool: protocol::Tool,
    pub tool_input: serde_json::Value,
    pub provider: String,
    pub request_type: protocol::RequestType,
    pub context: ApprovalContext,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            by_request_id: RwLock::new(HashMap::new()),
            by_session_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new approval or return existing one if request_id matches.
    pub async fn register(&self, params: RegisterApproval) -> Approval {
        // Idempotency: if request_id already registered, return existing
        {
            let by_req = self.by_request_id.read().await;
            if let Some(&existing_id) = by_req.get(&params.request_id) {
                let entries = self.entries.read().await;
                if let Some(entry) = entries.get(&existing_id) {
                    return entry.approval.clone();
                }
            }
        }

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
            created_at: Utc::now(),
            status: ApprovalStatus::Pending,
        };

        let (tx, _rx) = watch::channel(ApprovalStatus::Pending);
        let entry = ApprovalEntry {
            approval: approval.clone(),
            tx,
            last_polled_at: None,
        };

        {
            let mut entries = self.entries.write().await;
            entries.insert(id, entry);
        }
        {
            let mut by_req = self.by_request_id.write().await;
            by_req.insert(params.request_id, id);
        }
        {
            let mut by_sess = self.by_session_id.write().await;
            by_sess.entry(params.session_id).or_default().insert(id);
        }

        info!(approval_id = %id, "approval registered");
        approval
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
    pub async fn first_pending_for_session(&self, session_id: &str) -> Option<Approval> {
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
    pub async fn evict_session(&self, session_id: &str) {
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
            info!(approval_id = %id, session_id, "approval evicted with session");
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
        let mut entries = self.entries.write().await;
        let mut by_req = self.by_request_id.write().await;
        let mut by_sess = self.by_session_id.write().await;

        for approval in approvals {
            if approval.status.is_resolved() {
                continue;
            }
            let id = approval.id;
            let request_id = approval.request_id.clone();
            let session_id = approval.session_id.clone();

            let (tx, _rx) = watch::channel(ApprovalStatus::Pending);
            entries.insert(
                id,
                ApprovalEntry {
                    approval,
                    tx,
                    last_polled_at: None,
                },
            );
            by_req.insert(request_id, id);
            by_sess.entry(session_id).or_default().insert(id);
            info!(approval_id = %id, "approval restored");
        }
    }
}

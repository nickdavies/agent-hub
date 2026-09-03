use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{RwLock, watch};
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

// Re-export protocol types so existing `use super::approvals::X` imports work.
pub use protocol::{Approval, ApprovalStatus};

use protocol::{ApprovalRequest, SessionId};

struct ApprovalEntry {
    approval: Approval,
    /// Bounded stable JSON encoding of the complete typed request.
    canonical_request: Option<Box<str>>,
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
    by_session_id: RwLock<HashMap<SessionId, HashSet<Uuid>>>,
}

pub struct RegisterApproval {
    pub request: ApprovalRequest,
    pub project: String,
    pub canonical_request: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterApprovalError {
    #[error("request_id already exists with a different canonical request")]
    Conflict,
}

pub struct RegisterApprovalOutcome {
    pub approval: Approval,
    pub created: bool,
}

/// Persistence envelope. The legacy variant keeps existing state files readable.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ApprovalSnapshot {
    Current {
        approval: Approval,
        canonical_request: String,
    },
    Legacy(Approval),
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            by_request_id: RwLock::new(HashMap::new()),
            by_session_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new approval, or return the existing approval only when the
    /// request ID and canonical request bytes both match.
    pub async fn register(
        &self,
        params: RegisterApproval,
    ) -> Result<RegisterApprovalOutcome, RegisterApprovalError> {
        // Keep all indexes locked through comparison and insertion so no
        // observer can see a partially registered approval.
        let mut by_req = self.by_request_id.write().await;
        let mut entries = self.entries.write().await;
        let mut by_sess = self.by_session_id.write().await;
        if let Some(&existing_id) = by_req.get(&params.request.id)
            && let Some(entry) = entries.get_mut(&existing_id)
        {
            if let Some(existing) = &entry.canonical_request {
                if entry.approval.request_digest != params.request.request_digest
                    || existing.as_ref() != params.canonical_request
                {
                    return Err(RegisterApprovalError::Conflict);
                }
            } else if legacy_request_matches(&entry.approval, &params.request, &params.project) {
                // Legacy snapshots did not retain cwd or canonical request
                // bytes. A retry matching every retained field adopts the
                // complete request so subsequent retries are strict.
                entry.approval.request_digest = params.request.request_digest.clone();
                entry.canonical_request = Some(params.canonical_request.into_boxed_str());
            } else {
                return Err(RegisterApprovalError::Conflict);
            }
            return Ok(RegisterApprovalOutcome {
                approval: entry.approval.clone(),
                created: false,
            });
        }

        let id = Uuid::new_v4();
        let request = params.request;
        let approval = Approval {
            id,
            request_id: request.id.clone(),
            request_digest: request.request_digest,
            session_id: request.session_id.clone(),
            session_display_name: request.session_display_name,
            project: params.project,
            tool: request.tool,
            tool_input: request.tool_input,
            provider: request.provider,
            request_type: request.request_type,
            context: request.context,
            created_at: Utc::now(),
            status: ApprovalStatus::Pending,
        };

        let (tx, _rx) = watch::channel(ApprovalStatus::Pending);
        let entry = ApprovalEntry {
            approval: approval.clone(),
            canonical_request: Some(params.canonical_request.into_boxed_str()),
            tx,
            last_polled_at: None,
        };

        entries.insert(id, entry);
        by_req.insert(approval.request_id.clone(), id);
        by_sess
            .entry(approval.session_id.clone())
            .or_default()
            .insert(id);

        info!(approval_id = %id, "approval registered");
        Ok(RegisterApprovalOutcome {
            approval,
            created: true,
        })
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
        entry.tx.send_replace(status);
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
    pub async fn first_pending_for_session(&self, session_id: &SessionId) -> Option<Approval> {
        let entries = self.entries.read().await;
        let by_session = self.by_session_id.read().await;
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
    pub async fn snapshot(&self) -> Vec<ApprovalSnapshot> {
        let entries = self.entries.read().await;
        let mut pending: Vec<&ApprovalEntry> = entries
            .values()
            .filter(|e| !e.approval.status.is_resolved())
            .collect();
        pending.sort_by_key(|e| e.approval.created_at);
        pending
            .into_iter()
            .map(|entry| match &entry.canonical_request {
                Some(canonical_request) => ApprovalSnapshot::Current {
                    approval: entry.approval.clone(),
                    canonical_request: canonical_request.to_string(),
                },
                None => ApprovalSnapshot::Legacy(entry.approval.clone()),
            })
            .collect()
    }

    /// Restore approvals from persisted state.
    pub async fn restore(&self, approvals: Vec<ApprovalSnapshot>) {
        let mut by_req = self.by_request_id.write().await;
        let mut entries = self.entries.write().await;
        let mut by_sess = self.by_session_id.write().await;

        for snapshot in approvals {
            let (approval, canonical_request) = match snapshot {
                ApprovalSnapshot::Current {
                    approval,
                    canonical_request,
                } => (approval, Some(canonical_request.into_boxed_str())),
                ApprovalSnapshot::Legacy(approval) => (approval, None),
            };
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
                    canonical_request,
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

fn legacy_request_matches(approval: &Approval, request: &ApprovalRequest, project: &str) -> bool {
    approval.request_id == request.id
        && approval.session_id == request.session_id
        && approval.session_display_name == request.session_display_name
        && approval.project == project
        && approval.tool == request.tool
        && approval.tool_input == request.tool_input
        && approval.provider == request.provider
        && approval.request_type == request.request_type
        && approval.context == request.context
}

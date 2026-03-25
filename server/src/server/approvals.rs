use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, watch};
use tracing::info;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: Uuid,
    pub request_id: String,
    pub session_id: String,
    pub session_display_name: String,
    pub project: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub context: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: ApprovalStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved { message: Option<String> },
    Denied { reason: String },
    Superseded,
}

impl ApprovalStatus {
    pub fn is_resolved(&self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }
}

struct ApprovalEntry {
    approval: Approval,
    tx: watch::Sender<ApprovalStatus>,
}

pub struct ApprovalRegistry {
    entries: RwLock<HashMap<Uuid, ApprovalEntry>>,
    /// request_id -> approval Uuid (idempotency)
    by_request_id: RwLock<HashMap<String, Uuid>>,
    /// session_id -> approval Uuid (one per session)
    by_session_id: RwLock<HashMap<String, Uuid>>,
}

pub struct RegisterApproval {
    pub request_id: String,
    pub session_id: String,
    pub session_display_name: String,
    pub project: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub context: Option<String>,
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
    /// Supersedes any existing approval for the same session.
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

        // Supersede any existing approval for this session
        self.supersede_session(&params.session_id).await;

        let id = Uuid::new_v4();
        let approval = Approval {
            id,
            request_id: params.request_id.clone(),
            session_id: params.session_id.clone(),
            session_display_name: params.session_display_name,
            project: params.project,
            tool_name: params.tool_name,
            tool_input: params.tool_input,
            context: params.context,
            created_at: Utc::now(),
            status: ApprovalStatus::Pending,
        };

        let (tx, _rx) = watch::channel(ApprovalStatus::Pending);
        let entry = ApprovalEntry {
            approval: approval.clone(),
            tx,
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
            by_sess.insert(params.session_id, id);
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

    /// List only pending approvals.
    pub async fn list_pending(&self) -> Vec<Approval> {
        let entries = self.entries.read().await;
        entries
            .values()
            .filter(|e| !e.approval.status.is_resolved())
            .map(|e| e.approval.clone())
            .collect()
    }

    /// Supersede any existing pending approval for a session.
    async fn supersede_session(&self, session_id: &str) {
        let old_id = {
            let by_sess = self.by_session_id.read().await;
            by_sess.get(session_id).copied()
        };

        if let Some(old_id) = old_id {
            self.resolve(old_id, ApprovalStatus::Superseded).await;
            info!(approval_id = %old_id, session_id, "approval superseded");
        }
    }

    /// Remove all approvals for a session (on session eviction).
    pub async fn evict_session(&self, session_id: &str) {
        let approval_id = {
            let mut by_sess = self.by_session_id.write().await;
            by_sess.remove(session_id)
        };

        if let Some(id) = approval_id {
            self.resolve(id, ApprovalStatus::Superseded).await;
            // Clean up from entries and request_id index
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

    /// Export pending approvals for persistence.
    pub async fn snapshot(&self) -> Vec<Approval> {
        let entries = self.entries.read().await;
        entries
            .values()
            .filter(|e| !e.approval.status.is_resolved())
            .map(|e| e.approval.clone())
            .collect()
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
            entries.insert(id, ApprovalEntry { approval, tx });
            by_req.insert(request_id, id);
            by_sess.insert(session_id, id);
            info!(approval_id = %id, "approval restored");
        }
    }
}

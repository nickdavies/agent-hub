use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{RwLock, watch};
use tokio::time::Instant;
use tracing::info;
use uuid::Uuid;

pub use protocol::{PendingQuestion, QuestionStatus};

use protocol::SessionId;

struct QuestionEntry {
    question: PendingQuestion,
    tx: watch::Sender<QuestionStatus>,
    /// Tracks when a gateway last polled `/wait` for this question.
    last_polled_at: Option<Instant>,
}

pub struct QuestionRegistry {
    entries: RwLock<HashMap<Uuid, QuestionEntry>>,
    /// gateway request_id → question Uuid (idempotency)
    by_request_id: RwLock<HashMap<String, Uuid>>,
    /// session_id → question Uuids
    by_session_id: RwLock<HashMap<SessionId, HashSet<Uuid>>>,
}

pub struct RegisterQuestion {
    pub request_id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub project: String,
    pub question_request_id: String,
    pub questions: Vec<protocol::QuestionInfo>,
    pub provider: String,
}

impl QuestionRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            by_request_id: RwLock::new(HashMap::new()),
            by_session_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new question or return existing one if request_id matches.
    pub async fn register(&self, params: RegisterQuestion) -> PendingQuestion {
        // Idempotency
        {
            let by_req = self.by_request_id.read().await;
            if let Some(&existing_id) = by_req.get(&params.request_id) {
                let entries = self.entries.read().await;
                if let Some(entry) = entries.get(&existing_id) {
                    return entry.question.clone();
                }
            }
        }

        let id = Uuid::new_v4();
        let question = PendingQuestion {
            id,
            request_id: params.request_id.clone(),
            session_id: params.session_id.clone(),
            session_display_name: params.session_display_name,
            project: params.project,
            question_request_id: params.question_request_id,
            questions: params.questions,
            provider: params.provider,
            created_at: Utc::now(),
            status: QuestionStatus::Pending,
        };

        let (tx, _rx) = watch::channel(QuestionStatus::Pending);
        let entry = QuestionEntry {
            question: question.clone(),
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

        info!(question_id = %id, "question registered");
        question
    }

    /// Get a question by id.
    pub async fn get(&self, id: Uuid) -> Option<PendingQuestion> {
        let entries = self.entries.read().await;
        entries.get(&id).map(|e| e.question.clone())
    }

    /// Subscribe to status changes.
    pub async fn subscribe(&self, id: Uuid) -> Option<watch::Receiver<QuestionStatus>> {
        let entries = self.entries.read().await;
        entries.get(&id).map(|e| e.tx.subscribe())
    }

    /// Resolve a question (answer/reject/cancel).
    pub async fn resolve(&self, id: Uuid, status: QuestionStatus) -> Option<PendingQuestion> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(&id)?;
        if entry.question.status.is_resolved() {
            return Some(entry.question.clone());
        }
        entry.question.status = status.clone();
        let _ = entry.tx.send(status);
        Some(entry.question.clone())
    }

    /// List only pending questions, sorted by creation time (oldest first).
    pub async fn list_pending(&self) -> Vec<PendingQuestion> {
        let entries = self.entries.read().await;
        let mut pending: Vec<PendingQuestion> = entries
            .values()
            .filter(|e| !e.question.status.is_resolved())
            .map(|e| e.question.clone())
            .collect();
        pending.sort_by_key(|q| q.created_at);
        pending
    }

    /// Returns the first (oldest) pending question for the given session, if any.
    pub async fn first_pending_for_session(
        &self,
        session_id: &SessionId,
    ) -> Option<PendingQuestion> {
        let by_session = self.by_session_id.read().await;
        let entries = self.entries.read().await;
        by_session.get(session_id).and_then(|ids| {
            ids.iter()
                .filter_map(|id| entries.get(id))
                .filter(|e| !e.question.status.is_resolved())
                .min_by_key(|e| e.question.created_at)
                .map(|e| e.question.clone())
        })
    }

    /// Remove all questions for a session (on session eviction).
    pub async fn evict_session(&self, session_id: &SessionId) {
        let question_ids = {
            let mut by_sess = self.by_session_id.write().await;
            by_sess.remove(session_id).unwrap_or_default()
        };
        for id in question_ids {
            self.resolve(id, QuestionStatus::Cancelled).await;
            let request_id = {
                let mut entries = self.entries.write().await;
                entries.remove(&id).map(|e| e.question.request_id)
            };
            if let Some(req_id) = request_id {
                let mut by_req = self.by_request_id.write().await;
                by_req.remove(&req_id);
            }
            info!(question_id = %id, session_id = %session_id, "question evicted with session");
        }
    }

    /// Record that the gateway polled for this question (called from the wait handler).
    pub async fn touch(&self, id: Uuid) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&id) {
            entry.last_polled_at = Some(Instant::now());
        }
    }

    /// Cancel pending questions whose gateway has stopped polling.
    pub async fn evict_orphaned(&self, threshold: Duration) -> usize {
        let now = Instant::now();
        let orphaned_ids: Vec<Uuid> = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .filter(|(_, entry)| {
                    !entry.question.status.is_resolved()
                        && entry
                            .last_polled_at
                            .is_some_and(|t| now.duration_since(t) > threshold)
                })
                .map(|(&id, _)| id)
                .collect()
        };

        let count = orphaned_ids.len();
        for id in orphaned_ids {
            self.resolve(id, QuestionStatus::Cancelled).await;
            info!(question_id = %id, "question cancelled (orphaned — gateway stopped polling)");
        }
        count
    }
}

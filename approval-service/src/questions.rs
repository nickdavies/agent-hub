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
    /// Stable JSON encoding of the complete typed request.
    canonical_request: Option<Box<str>>,
    tx: watch::Sender<QuestionStatus>,
    /// Tracks when a gateway last polled `/wait` for this question.
    last_polled_at: Option<Instant>,
}

#[derive(Default)]
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
    pub canonical_request: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterQuestionError {
    #[error("request_id already exists with a different canonical request")]
    Conflict,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveQuestionError {
    #[error("question not found")]
    NotFound,
    #[error("answer count does not match question count")]
    AnswerCount,
    #[error("question {0} requires at least one answer")]
    EmptyAnswer(usize),
    #[error("question {0} does not allow multiple answers")]
    MultipleAnswers(usize),
    #[error("question {0} contains a duplicate answer")]
    DuplicateAnswer(usize),
    #[error("question {0} contains an invalid option or custom answer")]
    InvalidAnswer(usize),
}

pub struct RegisterQuestionOutcome {
    pub question: PendingQuestion,
    pub created: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum QuestionSnapshot {
    Current {
        question: PendingQuestion,
        canonical_request: String,
    },
    Legacy(PendingQuestion),
}

impl QuestionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new question or return an identical request already registered.
    pub async fn register(
        &self,
        params: RegisterQuestion,
    ) -> Result<RegisterQuestionOutcome, RegisterQuestionError> {
        let mut by_req = self.by_request_id.write().await;
        let mut entries = self.entries.write().await;
        let mut by_sess = self.by_session_id.write().await;
        if let Some(&existing_id) = by_req.get(&params.request_id)
            && let Some(entry) = entries.get_mut(&existing_id)
        {
            if entry.canonical_request.as_deref() == Some(params.canonical_request.as_str()) {
                return Ok(RegisterQuestionOutcome {
                    question: entry.question.clone(),
                    created: false,
                });
            }
            if entry.canonical_request.is_none() && legacy_request_matches(&entry.question, &params)
            {
                entry.canonical_request = Some(params.canonical_request.into_boxed_str());
                return Ok(RegisterQuestionOutcome {
                    question: entry.question.clone(),
                    created: false,
                });
            }
            return Err(RegisterQuestionError::Conflict);
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
            canonical_request: Some(params.canonical_request.into_boxed_str()),
            tx,
            last_polled_at: None,
        };

        entries.insert(id, entry);
        by_req.insert(params.request_id, id);
        by_sess.entry(params.session_id).or_default().insert(id);

        info!(question_id = %id, "question registered");
        Ok(RegisterQuestionOutcome {
            question,
            created: true,
        })
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
        entry.tx.send_replace(status);
        Some(entry.question.clone())
    }

    /// Validate and apply an answer against the immutable original questions.
    pub async fn answer(
        &self,
        id: Uuid,
        answers: Vec<Vec<String>>,
    ) -> Result<PendingQuestion, ResolveQuestionError> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(&id).ok_or(ResolveQuestionError::NotFound)?;
        validate_answers(&entry.question.questions, &answers)?;
        if entry.question.status.is_resolved() {
            return Ok(entry.question.clone());
        }
        let status = QuestionStatus::Answered { answers };
        entry.question.status = status.clone();
        entry.tx.send_replace(status);
        Ok(entry.question.clone())
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
        let entries = self.entries.read().await;
        let by_session = self.by_session_id.read().await;
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

    pub async fn snapshot(&self) -> Vec<QuestionSnapshot> {
        let entries = self.entries.read().await;
        let mut questions: Vec<&QuestionEntry> = entries.values().collect();
        questions.sort_by_key(|entry| entry.question.created_at);
        questions
            .into_iter()
            .map(|entry| match &entry.canonical_request {
                Some(canonical_request) => QuestionSnapshot::Current {
                    question: entry.question.clone(),
                    canonical_request: canonical_request.to_string(),
                },
                None => QuestionSnapshot::Legacy(entry.question.clone()),
            })
            .collect()
    }

    pub async fn restore(&self, questions: Vec<QuestionSnapshot>) {
        let mut by_req = self.by_request_id.write().await;
        let mut entries = self.entries.write().await;
        let mut by_sess = self.by_session_id.write().await;

        for snapshot in questions {
            let (question, canonical_request) = match snapshot {
                QuestionSnapshot::Current {
                    question,
                    canonical_request,
                } => (question, Some(canonical_request.into_boxed_str())),
                QuestionSnapshot::Legacy(question) => (question, None),
            };
            let id = question.id;
            let request_id = question.request_id.clone();
            let session_id = question.session_id.clone();
            let (tx, _rx) = watch::channel(question.status.clone());
            entries.insert(
                id,
                QuestionEntry {
                    question,
                    canonical_request,
                    tx,
                    last_polled_at: None,
                },
            );
            by_req.insert(request_id, id);
            by_sess.entry(session_id).or_default().insert(id);
            info!(question_id = %id, "question restored");
        }
    }
}

fn legacy_request_matches(question: &PendingQuestion, request: &RegisterQuestion) -> bool {
    question.request_id == request.request_id
        && question.session_id == request.session_id
        && question.session_display_name == request.session_display_name
        && question.project == request.project
        && question.question_request_id == request.question_request_id
        && serde_json::to_value(&question.questions).ok()
            == serde_json::to_value(&request.questions).ok()
        && question.provider == request.provider
}

fn validate_answers(
    questions: &[protocol::QuestionInfo],
    answers: &[Vec<String>],
) -> Result<(), ResolveQuestionError> {
    if answers.len() != questions.len() {
        return Err(ResolveQuestionError::AnswerCount);
    }
    for (index, (question, selected)) in questions.iter().zip(answers).enumerate() {
        if selected.is_empty() {
            return Err(ResolveQuestionError::EmptyAnswer(index));
        }
        if !question.multiple.unwrap_or(false) && selected.len() != 1 {
            return Err(ResolveQuestionError::MultipleAnswers(index));
        }
        let mut unique = HashSet::new();
        for answer in selected {
            if !unique.insert(answer) {
                return Err(ResolveQuestionError::DuplicateAnswer(index));
            }
            let is_label = question
                .options
                .iter()
                .any(|option| option.label == *answer);
            let is_custom = question.custom.unwrap_or(true) && !answer.trim().is_empty();
            if !is_label && !is_custom {
                return Err(ResolveQuestionError::InvalidAnswer(index));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn request(canonical_request: &str) -> RegisterQuestion {
        RegisterQuestion {
            request_id: "request-1".into(),
            session_id: SessionId::new("session-1"),
            session_display_name: "Session".into(),
            project: "workspace".into(),
            question_request_id: "question-1".into(),
            questions: vec![],
            provider: "opencode".into(),
            canonical_request: canonical_request.into(),
        }
    }

    #[tokio::test]
    async fn concurrent_registration_is_atomic_and_conflicts_are_rejected() {
        let registry = Arc::new(QuestionRegistry::new());
        let first_registry = Arc::clone(&registry);
        let second_registry = Arc::clone(&registry);
        let (first, second) = tokio::join!(
            async move { first_registry.register(request("canonical")).await.unwrap() },
            async move {
                second_registry
                    .register(request("canonical"))
                    .await
                    .unwrap()
            }
        );
        assert_eq!(first.question.id, second.question.id);
        assert_ne!(first.created, second.created);
        assert!(matches!(
            registry.register(request("different")).await,
            Err(RegisterQuestionError::Conflict)
        ));
    }

    #[tokio::test]
    async fn snapshot_restore_preserves_strict_identity() {
        let original = QuestionRegistry::new();
        let registered = original
            .register(request("canonical"))
            .await
            .unwrap()
            .question;
        let restored = QuestionRegistry::new();
        restored.restore(original.snapshot().await).await;
        assert_eq!(
            restored
                .register(request("canonical"))
                .await
                .unwrap()
                .question
                .id,
            registered.id
        );
        assert!(matches!(
            restored.register(request("different")).await,
            Err(RegisterQuestionError::Conflict)
        ));
    }

    #[tokio::test]
    async fn invalid_answers_do_not_resolve_the_question() {
        let registry = QuestionRegistry::new();
        let mut params = request("canonical");
        params.questions = vec![protocol::QuestionInfo {
            question: "Pick".into(),
            header: "Pick".into(),
            options: vec![protocol::QuestionOption {
                label: "one".into(),
                description: "One".into(),
            }],
            multiple: Some(false),
            custom: Some(false),
        }];
        let id = registry.register(params).await.unwrap().question.id;

        assert!(matches!(
            registry
                .answer(id, vec![vec!["one".into(), "one".into()]])
                .await,
            Err(ResolveQuestionError::MultipleAnswers(0))
        ));
        assert!(matches!(
            registry.answer(id, vec![vec!["custom".into()]]).await,
            Err(ResolveQuestionError::InvalidAnswer(0))
        ));
        assert_eq!(
            registry.get(id).await.unwrap().status,
            QuestionStatus::Pending
        );
    }
}

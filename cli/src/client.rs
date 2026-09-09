use agent_hub_approval_client::ApprovalClient;
use protocol::{
    ApprovalDecision, ApprovalResolveRequest, PendingQuestion, QuestionDecision,
    QuestionResolveRequest, Secret,
};
use uuid::Uuid;

pub use protocol::Approval;
pub use protocol::PendingQuestion as Question;

pub struct Client {
    inner: ApprovalClient,
}

impl Client {
    pub fn new(base_url: String, token: Option<Secret>) -> Result<Self, String> {
        ApprovalClient::new(&base_url, token)
            .map(|inner| Self { inner })
            .map_err(|error| error.to_string())
    }

    pub async fn list_pending(&self) -> Result<Vec<Approval>, String> {
        self.inner
            .list_pending_approvals()
            .await
            .map(|response| response.value)
            .map_err(|error| error.to_string())
    }

    pub async fn list_pending_questions(&self) -> Result<Vec<PendingQuestion>, String> {
        self.inner
            .list_pending_questions()
            .await
            .map(|response| response.value)
            .map_err(|error| error.to_string())
    }

    pub async fn approve(&self, id: Uuid, message: Option<String>) -> Result<(), String> {
        self.resolve(id, ApprovalDecision::Approve, message).await
    }

    pub async fn deny(&self, id: Uuid, reason: Option<String>) -> Result<(), String> {
        self.resolve(id, ApprovalDecision::Deny, reason).await
    }

    async fn resolve(
        &self,
        id: Uuid,
        decision: ApprovalDecision,
        message: Option<String>,
    ) -> Result<(), String> {
        self.inner
            .resolve_approval_status(id, &ApprovalResolveRequest { decision, message })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub async fn answer_question(&self, id: Uuid, answers: Vec<Vec<String>>) -> Result<(), String> {
        self.resolve_question(
            id,
            QuestionResolveRequest {
                decision: QuestionDecision::Answer,
                answers: Some(answers),
                reason: None,
            },
        )
        .await
    }

    pub async fn reject_question(&self, id: Uuid) -> Result<(), String> {
        self.resolve_question(
            id,
            QuestionResolveRequest {
                decision: QuestionDecision::Reject,
                answers: None,
                reason: None,
            },
        )
        .await
    }

    async fn resolve_question(
        &self,
        id: Uuid,
        request: QuestionResolveRequest,
    ) -> Result<(), String> {
        self.inner
            .resolve_question_status(id, &request)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub async fn health_check(&self) -> Result<(), String> {
        self.inner
            .health()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub async fn check_auth(&self) -> Result<u16, String> {
        self.inner
            .session_status()
            .await
            .map_err(|error| error.to_string())
    }
}

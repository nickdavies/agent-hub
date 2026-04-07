use std::time::Duration;

use protocol::{
    ApprovalDecision, ApprovalResolveRequest, PendingQuestion, QuestionDecision,
    QuestionResolveRequest, Secret,
};
use uuid::Uuid;

// Re-export types so existing `use crate::client::X` imports work.
pub use protocol::Approval;
pub use protocol::PendingQuestion as Question;

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    token: Option<Secret>,
}

impl Client {
    pub fn new(base_url: String, token: Option<Secret>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            base_url,
            token,
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.bearer_auth(token.expose())
        } else {
            req
        }
    }

    pub async fn list_pending(&self) -> Result<Vec<Approval>, String> {
        let url = format!("{}/api/v1/approvals/pending", self.base_url);
        let req = self.auth(self.http.get(&url));
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        resp.json()
            .await
            .map_err(|e| format!("failed to parse response: {e}"))
    }

    pub async fn list_pending_questions(&self) -> Result<Vec<PendingQuestion>, String> {
        let url = format!("{}/api/v1/questions/pending", self.base_url);
        let req = self.auth(self.http.get(&url));
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        resp.json()
            .await
            .map_err(|e| format!("failed to parse response: {e}"))
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
        let url = format!("{}/api/v1/approvals/{}/resolve", self.base_url, id);
        let req = self
            .auth(self.http.post(&url))
            .json(&ApprovalResolveRequest { decision, message });
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        Ok(())
    }

    pub async fn answer_question(&self, id: Uuid, answers: Vec<Vec<String>>) -> Result<(), String> {
        let url = format!("{}/api/v1/questions/{}/resolve", self.base_url, id);
        let req = self
            .auth(self.http.post(&url))
            .json(&QuestionResolveRequest {
                decision: QuestionDecision::Answer,
                answers: Some(answers),
                reason: None,
            });
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        Ok(())
    }

    pub async fn reject_question(&self, id: Uuid) -> Result<(), String> {
        let url = format!("{}/api/v1/questions/{}/resolve", self.base_url, id);
        let req = self
            .auth(self.http.post(&url))
            .json(&QuestionResolveRequest {
                decision: QuestionDecision::Reject,
                answers: None,
                reason: None,
            });
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        Ok(())
    }

    pub async fn health_check(&self) -> Result<(), String> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("connection failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{} returned {}", url, resp.status()));
        }
        Ok(())
    }

    /// Check that the server accepts our auth token by hitting an authenticated endpoint.
    /// Returns the HTTP status code on success, or an error string on connection failure.
    pub async fn check_auth(&self) -> Result<u16, String> {
        let url = format!("{}/api/v1/sessions", self.base_url);
        let req = self.auth(self.http.get(&url));
        let resp = req
            .send()
            .await
            .map_err(|e| format!("connection failed: {e}"))?;
        Ok(resp.status().as_u16())
    }
}

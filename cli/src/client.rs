use std::time::Duration;

use capabilities::ApprovalContext;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct Approval {
    pub id: Uuid,
    #[allow(dead_code)]
    pub session_id: String,
    pub session_display_name: String,
    pub project: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub context: ApprovalContext,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct ResolveRequest {
    decision: &'static str,
    message: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: String, token: Option<String>) -> Self {
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
            req.bearer_auth(token)
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

    pub async fn approve(&self, id: Uuid, message: Option<String>) -> Result<(), String> {
        self.resolve(id, "approve", message).await
    }

    pub async fn deny(&self, id: Uuid, reason: Option<String>) -> Result<(), String> {
        self.resolve(id, "deny", reason).await
    }

    async fn resolve(
        &self,
        id: Uuid,
        decision: &'static str,
        message: Option<String>,
    ) -> Result<(), String> {
        let url = format!("{}/api/v1/approvals/{}/resolve", self.base_url, id);
        let req = self
            .auth(self.http.post(&url))
            .json(&ResolveRequest { decision, message });
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

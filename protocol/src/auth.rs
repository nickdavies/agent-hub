use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a credential may do. OIDC/basic sessions are always `Reviewer`;
/// bearer tokens declare their role in the server's `AGENT_HUB_TOKENS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Raise, monitor and cancel approvals/questions. Cannot resolve them.
    Agent,
    /// Full access, including approve/deny/answer/reject.
    Reviewer,
}

/// GET /api/v1/whoami — the authenticated caller.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhoAmIResponse {
    /// Token label, or email for OIDC/basic sessions.
    pub label: String,
    pub role: Role,
}

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sessions::SessionId;

// ===========================================================================
// Question content types (mirrors opencode's QuestionInfo / QuestionOption)
// ===========================================================================

/// A single selectable option within a question.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// One question with its choices.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionInfo {
    pub question: String,
    /// Short label (≤30 chars) shown as the tab/step heading.
    pub header: String,
    pub options: Vec<QuestionOption>,
    /// If true, multiple options may be selected simultaneously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// If true (default), a free-text "Type your own answer" input is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

// ===========================================================================
// Status enum
// ===========================================================================

/// The lifecycle state of a proxied question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionStatus {
    /// Waiting for the operator to answer.
    Pending,
    /// Operator submitted answers.
    Answered {
        /// One `Answer` (array of selected labels) per question, in order.
        answers: Vec<Vec<String>>,
    },
    /// Operator dismissed the question.
    Rejected { reason: Option<String> },
    /// Gateway timed out or was killed without resolving.
    Cancelled,
}

impl QuestionStatus {
    pub fn is_resolved(&self) -> bool {
        !matches!(self, QuestionStatus::Pending)
    }
}

// ===========================================================================
// Gateway → Server: POST /api/v1/hooks/question
// ===========================================================================

/// Request body sent by the gateway when a question needs proxying.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionProxyRequest {
    /// Gateway-generated UUID used as an idempotency key.
    pub id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub cwd: String,
    /// The opencode-native question ID (e.g. "que_01j...").
    pub question_request_id: String,
    pub questions: Vec<QuestionInfo>,
    /// Provider that originated the question (always "opencode" for now).
    pub provider: String,
}

// ===========================================================================
// Server → Gateway: response to POST /api/v1/hooks/question
// ===========================================================================

/// Immediate response returned to the gateway after registering a question.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionProxyResponse {
    pub id: Uuid,
    #[serde(flatten)]
    pub status: QuestionStatus,
}

// ===========================================================================
// GET /api/v1/questions/{id}/wait — long-poll response
// ===========================================================================

/// Response from the long-poll wait endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionWaitResponse {
    #[serde(flatten)]
    pub status: QuestionStatus,
}

// ===========================================================================
// Human → Server: POST /api/v1/questions/{id}/resolve
// ===========================================================================

/// Decision choices when resolving a question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionDecision {
    /// Operator submitted answers.
    Answer,
    /// Operator dismissed / rejected the question.
    Reject,
    /// Gateway cancelled (e.g. session ended).
    Cancel,
}

/// Request body for POST /api/v1/questions/{id}/resolve.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionResolveRequest {
    pub decision: QuestionDecision,
    /// Required when `decision == Answer`. One array of selected labels per question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<Vec<String>>>,
    /// Optional reason shown in the dashboard (used with `Reject` or `Cancel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ===========================================================================
// Gateway stdout: exit-0 output of the `question` subcommand
// ===========================================================================

/// JSON written to stdout by `agent-hub-gateway question` on exit 0.
///
/// Exit codes:
///   0 = answered   — stdout contains this struct
///   1 = rejected, cancelled, timed out, or server unreachable
///   2 = fail-closed (bad input / internal error)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuestionGatewayOutput {
    /// One array of selected labels per question, in input order.
    pub answers: Vec<Vec<String>>,
}

/// A pending (or resolved) question as stored by the server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PendingQuestion {
    /// Server-assigned UUID.
    pub id: Uuid,
    /// Gateway-generated idempotency key (matches `QuestionProxyRequest::id`).
    pub request_id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub project: String,
    /// The opencode-native question ID (forwarded to the plugin for reply/reject calls).
    pub question_request_id: String,
    pub questions: Vec<QuestionInfo>,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub status: QuestionStatus,
}

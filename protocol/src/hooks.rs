use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::sessions::{Provider, SessionStatus};

/// Common fields from hook payloads (stop, notification, session-end).
///
/// Serde ignores unknown fields by default, so providers can send extra fields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub message: Option<String>,
    pub editor_type: Option<Provider>,
}

/// POST /api/v1/hooks/status — request body sent by the gateway (status-report subcommand).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusReport {
    pub session_id: String,
    pub cwd: String,
    pub status: SessionStatus,
    pub waiting_reason: Option<String>,
    pub display_name: Option<String>,
    pub editor_type: Option<Provider>,
}

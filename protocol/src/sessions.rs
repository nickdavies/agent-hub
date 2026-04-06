use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

/// The editor/agent that owns a session.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    JsonSchema,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Provider {
    Claude,
    Cursor,
    Opencode,
    #[default]
    Unknown,
}

/// Backwards-compatible alias while downstream code migrates.
pub type EditorType = Provider;

/// Stored session status as reported by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Active,
    Idle,
    Waiting,
    Ended,
}

/// Effective session status after server-side resolution (e.g. pending approvals
/// override to Waiting). Used in API responses to the web UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum EffectiveSessionStatus {
    Active,
    Idle,
    Waiting { reason: Option<String> },
    Ended,
}

/// Per-session approval mode: remote (web UI) or terminal (CLI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionApprovalMode {
    #[default]
    Remote,
    Terminal,
}

/// Per-session notification and approval config.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionNotifyConfig {
    pub stop_enabled: bool,
    pub permission_enabled: bool,
    #[serde(default)]
    pub approval_mode: SessionApprovalMode,
}

impl SessionNotifyConfig {
    pub fn with_default_approval_mode(mode: SessionApprovalMode) -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
            approval_mode: mode,
        }
    }
}

impl Default for SessionNotifyConfig {
    fn default() -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
            approval_mode: SessionApprovalMode::default(),
        }
    }
}

/// Partial update for per-session config (PUT /api/v1/sessions/{id}).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionConfigUpdate {
    pub stop_enabled: Option<bool>,
    pub permission_enabled: Option<bool>,
    pub approval_mode: Option<SessionApprovalMode>,
}

impl SessionNotifyConfig {
    pub fn apply(&mut self, update: &SessionConfigUpdate) {
        if let Some(v) = update.stop_enabled {
            self.stop_enabled = v;
        }
        if let Some(v) = update.permission_enabled {
            self.permission_enabled = v;
        }
        if let Some(v) = update.approval_mode {
            self.approval_mode = v;
        }
    }
}

/// API response for a session (with effective status resolved).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub session_id: String,
    pub project: String,
    pub config: SessionNotifyConfig,
    pub editor_type: Provider,
    pub status: EffectiveSessionStatus,
    pub display_name: Option<String>,
}

/// GET /api/v1/sessions/{id}/approval-mode — response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalModeResponse {
    pub approval_mode: SessionApprovalMode,
}

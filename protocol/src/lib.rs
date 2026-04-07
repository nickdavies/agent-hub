//! Shared wire-protocol types for the agent-hub system.
//!
//! This crate is the single source of truth for all JSON structures exchanged
//! between the gateway, server, CLI, and provider plugins.
//!
//! All public types derive `schemars::JsonSchema` so that JSON schema files can
//! be generated for TypeScript plugin consumption.

pub mod approval;
pub mod config;
pub mod gateway;
pub mod hooks;
pub mod presence;
pub mod secret;
pub mod sessions;
pub mod tool;
pub mod tool_call;

// Re-export the most commonly used types at the crate root for convenience.
pub use approval::{
    Approval, ApprovalContext, ApprovalDecision, ApprovalRequest, ApprovalResolveRequest,
    ApprovalResponse, ApprovalStatus, ApprovalWaitResponse, ExtraContext, HookEventName,
    KnownHookEvent, RequestType,
};
pub use config::{ConfigResponse, NotifyConfig, NotifyConfigUpdate};
pub use gateway::{
    ClaudeCodeHookInput, ClaudePermissionBehavior, ClaudePermissionRequestDecision,
    ClaudePermissionRequestOutput, ClaudePreToolUseDecision, ClaudePreToolUseOutput, ClaudeTool,
    CursorHookInput, CursorHookOutput, CursorSessionKey, CursorTool, DelegateOutput,
    DelegateOutputDecision, DelegatePayload, DelegatePermission, OpenCodeHookInput,
    OpenCodeHookOutput, OpenCodeTool, PermissionDecision,
};
pub use hooks::{NotificationPayload, SessionEndPayload, StatusReport, StopPayload};
pub use presence::{PresenceState, PresenceUpdate};
pub use secret::Secret;
pub use sessions::{
    ApprovalModeResponse, EditorType, EffectiveSessionStatus, Provider, SessionApprovalMode,
    SessionConfigUpdate, SessionId, SessionNotifyConfig, SessionStatus, SessionView,
};
pub use tool::{expand_tool_group, Tool, ToolCategory};
pub use tool_call::{MultiEditEntry, ToolCall, ToolCallKind, ToolCallParseError};

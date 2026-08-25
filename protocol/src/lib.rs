//! Shared wire-protocol types for the agent-hub system.
//!
//! This crate is the single source of truth for all JSON structures exchanged
//! between the gateway, server, CLI, and provider plugins.
//!
//! All public types derive `schemars::JsonSchema` so that JSON schema files can
//! be generated for TypeScript plugin consumption.

pub mod approval;
pub mod attended;
pub mod config;
pub mod gateway;
pub mod hooks;
pub mod job_events;
pub mod orchestration;
pub mod presence;
pub mod question;
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
pub use attended::{
    APPROVED_EXECUTION_PIDS_LIMIT, ATTENDED_AUTHORIZATION_REQUEST_VERSION,
    ATTENDED_AUTHORIZATION_VERSION, ApprovedExecutionSpec, AttendedAuthorizationArtifact,
    AttendedAuthorizationRequestV1, AttendedExecutionSpec, AttendedLifecycleContract,
    AttendedOutputContract, AttendedOutputLimits, AttendedProposal, AuthorizedCredentialProvider,
    AuthorizedOverlay, ContextRootSpec, RepositoryMountSpec, RepositoryState, ToolApprovalMode,
    WorkspaceAccess, WorkspaceSpec, canonical_context_root_path, is_canonical_context_root_path,
    sha256_hex_with_domain,
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
pub use job_events::{JOB_EVENT_SCHEMA_NAME, JobEvent, JobEventKind};
pub use orchestration::{
    AGENT_CONTEXT_SCHEMA_NAME, AGENT_CONTEXT_SCHEMA_VERSION, AGENT_OUTPUT_SCHEMA_NAME,
    AUTHORIZATION_REQUEST_VERSION, AgentContext, AgentContextArtifacts, AgentContextJob,
    AgentContextPaths, AgentContextRepos, AgentContextWorkflow, AgentOutput, AgentOutputError,
    AgentOutputEvent, AgentOutputGitHubPr, ApprovalRuleSet, ArtifactSummary,
    AuthenticatedWorkerSession, BlockReason, CANONICAL_AUTHORIZATION_VERSION,
    CANONICAL_AUTHORIZATION_VERSION_V2, CANONICAL_AUTHORIZATION_VERSION_V3,
    CANONICAL_AUTHORIZATION_VERSION_V4, CONTEXT_AUTHORIZATION_ID_KEY,
    CONTEXT_AUTHORIZATION_REQUEST_KEY, CONTEXT_REQUEST_DIGEST_KEY, CancellationAcknowledgement,
    CancellationCommand, CancellationDeliveryState, CancellationLifecycleState,
    CancellationOutcome, CancellationStatus, CanonicalAuthorizationArtifact, CanonicalRuntimeSpec,
    ContainmentAbsenceEvidence, ContainmentStatus, CreateWorkItemRequest, EnvironmentDefinition,
    EnvironmentOverlay, EnvironmentSelection, EventSource, ExecutionMode, ExecutionOutcome,
    ExecutionTiming, FireEventRequest, FireEventResponse, GithubAccessClass, GithubAction,
    GithubCapability, GithubCapabilityRequest, GithubOperation, GithubPullRequestIdentity,
    GithubPullRequestMetadata, GithubRepositoryIdentity, HeartbeatRequest, HeartbeatResponse,
    ImmutableAuthorizationBundle, JOB_RUN_SCHEMA_NAME, Job, JobResult, JobState, JobUpdateRequest,
    MergePolicy, NetworkResourceKind, NetworkResourceRequest, PlannedAgentJobView,
    PlannedEnvironmentSpec, PlannedEnvironmentTier, PlannedPermissionSet, PlannedPermissionTier,
    PlannedRepoClaim, PlannedWorkItemView, PollJobRequest, PollJobResponse, PromptBundle,
    PromptRef, ProviderIdentity, ProviderOwnership, ReactiveAgentStateConfig,
    ReactiveAutoStateConfig, ReactiveCommandStateConfig, ReactiveEventKind,
    ReactiveExecutionConfig, ReactiveHumanStateConfig, ReactiveManagedStateConfig,
    ReactiveStateConfig, ReactiveStateKind, ReactiveTerminalOutcome, ReactiveTerminalStateConfig,
    ReactiveWorkflowDefinition, ReactiveWorkflowState, ReactiveWorkflowTransition,
    RegisterWorkerRequest, RegisterWorkerResponse, RepoAccessMode, RepoExecutionMetadata,
    RepoSource, ResolvedAuthorizationBehavior, ResolvedAuthorizationProvider,
    ResolvedAuthorizationRequest, ResolvedAuthorizationResources, StructuredTranscriptArtifact,
    TerminationReason, WorkItem, WorkItemState, WorkerInfo, WorkerProtocolCapability,
    WorkerSessionCredential, WorkerState, WorkflowEvent, WorkflowEventKind, WorkflowEventPayload,
    WorkflowValidationError, WorkflowValidationReport, authorization_execution_context,
    authorization_id_for_request, authorization_request_from_job_context,
    authorized_default_branch, cancellation_assignment_id, canonical_environment_for_execution,
    canonical_github_api_base_for_host, canonical_github_api_base_url, canonical_github_host,
    canonical_permissions_for_environment, canonical_runtime_spec_for_execution,
    canonical_runtime_spec_for_job, canonical_runtime_spec_for_job_effective,
    default_approval_rules_for_tier, parse_github_pull_request_url, parse_github_repository_url,
    parse_workflow_toml, planned_agent_job_view_from_prototype,
    planned_work_item_view_from_prototype, project_authorization_bundle_into_job_context,
    project_runtime_spec_into_job_context, reactive_event_kind_from_str,
    runtime_spec_from_job_context, validate_agent_output_event, validate_reactive_workflow,
    validate_reactive_workflows, verify_cancellation_assignment_id,
    worker_capability_for_canonical_authorization_version, worker_supports_canonical_authorization,
    workflow_event_is_externally_fireable, workflow_event_is_internal_system_only,
};
pub use presence::{PresenceState, PresenceUpdate};
pub use question::{
    PendingQuestion, QuestionDecision, QuestionGatewayOutput, QuestionInfo, QuestionOption,
    QuestionProxyRequest, QuestionProxyResponse, QuestionResolveRequest, QuestionStatus,
    QuestionWaitResponse,
};
pub use secret::Secret;
pub use sessions::{
    ApprovalModeResponse, EditorType, EffectiveSessionStatus, Provider, SessionApprovalMode,
    SessionConfigUpdate, SessionId, SessionNotifyConfig, SessionStatus, SessionView,
};
pub use tool::{Tool, ToolCategory, expand_tool_group};
pub use tool_call::{MultiEditEntry, ToolCall, ToolCallKind, ToolCallParseError};

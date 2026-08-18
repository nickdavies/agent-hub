use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use protocol::{
    CreateWorkItemRequest, EnvironmentSelection, FireEventRequest, ProviderIdentity,
    ProviderOwnership, RepoSource, WorkItem, WorkItemState, WorkflowEventKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TaskAdapterError {
    #[error("external task not found: {task_id}")]
    TaskNotFound { task_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTriageStatus {
    NeedsWorkflowSelection,
    Ready,
    InProgress,
    Blocked,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanActionKind {
    ChooseWorkflow,
    Submit,
    Approve,
    RequestChanges,
    Unblock,
    Retry,
    Cancel,
    /// Human declares CI passed for this PR. Maps to
    /// `WorkflowEventKind::ChecksPassed` so workflows already wired for
    /// the poller's `checks_passed` event work without changes.
    ChecksPassed,
    /// Human declares CI failed for this PR. Maps to
    /// `WorkflowEventKind::ChecksFailed`.
    ChecksFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHumanAction {
    pub kind: HumanActionKind,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskToolRecord {
    pub id: Uuid,
    #[serde(default)]
    pub ownership: ProviderOwnership,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    pub triage_status: TaskTriageStatus,
    #[serde(default)]
    pub pending_human_action: bool,
    #[serde(default)]
    pub current_human_gate: Option<String>,
    #[serde(default)]
    pub workflow_phase: Option<String>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub last_failure_reason: Option<String>,
    #[serde(default)]
    pub required_action_description: Option<String>,
    #[serde(default)]
    pub human_gate_state: Option<TaskHumanGateState>,
    #[serde(default)]
    pub valid_next_events: Vec<WorkflowEventKind>,
    #[serde(default)]
    pub allowed_actions: Vec<HumanActionKind>,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub repo: Option<RepoSource>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub requested_action: Option<TaskHumanAction>,
    #[serde(default)]
    pub action_revision: u64,
}

#[derive(Debug, Clone)]
pub struct TaskSyncRecord {
    pub external: TaskToolRecord,
    pub ownership: ProviderOwnership,
    pub previous_ownership: Option<ProviderOwnership>,
    pub ownership_revision: u64,
    pub provider_rejection: Option<TaskProviderRejection>,
}

#[derive(Debug, Clone)]
pub struct TaskSyncError {
    pub task_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct TaskSyncSnapshot {
    pub records: Vec<TaskSyncRecord>,
    pub complete: bool,
    pub errors: Vec<TaskSyncError>,
    pub listed_task_ids: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct TaskProviderRejection {
    pub action_revision: u64,
    pub action_payload: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum AdapterIntent {
    Create {
        task: TaskToolRecord,
        ownership: ProviderOwnership,
        previous_ownership: Option<ProviderOwnership>,
    },
    UpdateProjection {
        work_item_id: Uuid,
        task: TaskToolRecord,
    },
    ApplyAction {
        work_item_id: Uuid,
        action: TaskHumanAction,
        action_revision: u64,
    },
    RejectAction {
        work_item_id: Option<Uuid>,
        task_id: String,
        action_revision: u64,
        action_payload: String,
        reason: String,
        source: ProviderActionRejectionSource,
    },
    OwnershipChanged {
        work_item_id: Uuid,
        task: TaskToolRecord,
        ownership: ProviderOwnership,
        baseline_only: bool,
        ownership_revision: u64,
    },
    CancelMissingExternalTask {
        work_item_id: Uuid,
        task_id: String,
    },
}

#[derive(Debug, Clone)]
pub enum ProviderActionRejectionSource {
    Command,
    Ownership {
        ownership: ProviderOwnership,
        intent: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct TaskProjection {
    pub task: TaskToolRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum TaskChangeType {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TaskChange {
    pub task_id: String,
    pub change_type: TaskChangeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TaskUpdate {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TaskCreate {
    pub title: String,
    pub description: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    pub current_step: String,
    pub phase: String,
    pub workflow_id: String,
    pub repo: Option<String>,
    pub pr_url: Option<String>,
    pub pr_status: Option<String>,
    pub human_gate: bool,
    pub failure_info: Option<String>,
    pub updated_at: DateTime<Utc>,
    // Orchestrator-owned state mirrored into the checkpoint so it survives
    // a Vikunja round-trip. Both are stored as raw `Option<String>` rather
    // than enum-typed because the orchestrator uses some state names that
    // are not variants of `TaskHumanGateState` / `TaskTriageStatus` (for
    // example `triage_completed`, `pending_approval`, `selected`). Storing
    // raw strings keeps the round-trip lossless without forcing enum
    // surgery elsewhere. Defaults preserve checkpoints written before these
    // fields existed.
    #[serde(default)]
    pub human_gate_state: Option<String>,
    #[serde(default)]
    pub triage_status: Option<String>,
    /// Human who should regain provider ownership when checkpointing or
    /// recovery fails. This is authoritative orchestration state, not merely
    /// an informational assignee projection.
    #[serde(default)]
    pub human_owner: Option<String>,
    /// Stable provider-native identity for `human_owner` when available.
    #[serde(default)]
    pub human_owner_provider_id: Option<String>,
    /// Complete provider-native human owner set. Scalar owner fields remain
    /// for backward compatibility with checkpoints written before this field.
    #[serde(default)]
    pub human_owners: Vec<ProviderIdentity>,
    /// Exact staged jobs authorized by this checkpoint commit. A job staged
    /// concurrently after the checkpoint snapshot remains gated.
    #[serde(default)]
    pub dispatch_job_ids: Vec<Uuid>,
    #[serde(default)]
    pub dispatch_authorizations: BTreeMap<Uuid, String>,
    #[serde(default)]
    pub authorization_pending: Option<CheckpointAuthorizationPending>,
    #[serde(default)]
    pub proposed_authorizations: Vec<serde_json::Value>,
    /// Monotonic per-root generation used by the durable checkpoint outbox.
    #[serde(default)]
    pub generation: u64,
    /// SHA-256 of the checkpoint with this field blank. Used to reconcile a
    /// provider commit after a process crash without overwriting it.
    #[serde(default)]
    pub checkpoint_hash: String,
    /// Stable hash of semantic state, excluding timestamps, generation, and
    /// the exact-payload hash. Used to make reconciliation idempotent.
    #[serde(default)]
    pub semantic_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointAuthorizationPending {
    pub work_item_id: Uuid,
    pub request_digest: String,
    pub request: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageResult {
    pub workflow_id: Option<String>,
    pub repo: Option<String>,
    pub environment: Option<EnvironmentSelection>,
    pub summary: String,
    pub confidence: Option<f64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub complexity: Option<String>,
    #[serde(default)]
    pub plan_files: Option<Vec<String>>,
    #[serde(default)]
    pub plan_approach: Option<Vec<String>>,
    #[serde(default)]
    pub plan_risks: Option<Vec<String>>,
    #[serde(default)]
    pub agent_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskLabels {
    pub workflow: Option<String>,
    pub step: Option<String>,
    pub phase: Option<String>,
    pub failed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProviderActionNamespace {
    Command,
    Ownership,
}

impl ProviderActionNamespace {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Ownership => "ownership",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "command" => Ok(Self::Command),
            "ownership" => Ok(Self::Ownership),
            _ => anyhow::bail!("unknown provider action namespace '{value}'"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProviderActionIdentity {
    pub namespace: ProviderActionNamespace,
    pub revision: u64,
}

pub struct ProviderActionRejectionDelivery<'a> {
    pub identity: &'a ProviderActionIdentity,
    pub legacy_revision: Option<u64>,
    pub reason: &'a str,
    pub valid_actions: &'a [HumanActionKind],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskHumanGateState {
    AwaitingHumanAction,
    NeedsWorkflowSelection,
    WorkflowSelected,
    Approved,
    ChangesRequested,
    Unblocked,
    RetryRequested,
}

pub trait TaskAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;
    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>>;
    fn pull_task_snapshot(&self) -> anyhow::Result<TaskSyncSnapshot> {
        let records = self.pull_tasks()?;
        Ok(TaskSyncSnapshot {
            listed_task_ids: records.iter().map(|record| record.external.id).collect(),
            records,
            complete: true,
            errors: Vec::new(),
        })
    }
    fn get_task(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>>;
    fn persist_projection(
        &self,
        projection: &TaskProjection,
        status_comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>>;

    #[allow(dead_code)]
    fn poll_changes(&self) -> anyhow::Result<Vec<TaskChange>> {
        Err(anyhow::anyhow!("not implemented"))
    }

    #[allow(dead_code)]
    fn update_task(&self, _task_id: &str, _update: TaskUpdate) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("not implemented"))
    }

    #[allow(dead_code)]
    fn create_task(&self, _create: TaskCreate) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("not implemented"))
    }

    /// Update or create the checkpoint comment on the task
    fn update_checkpoint(
        &self,
        _task_id: &str,
        _checkpoint: &TaskCheckpoint,
        _work_item_id: Uuid,
        _comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support authoritative checkpoints",
            self.adapter_name()
        ))
    }

    /// Append a log entry comment to the task
    fn append_log(&self, _task_id: &str, _message: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support provider-visible logs",
            self.adapter_name()
        ))
    }

    /// Write/update a structured job result comment on the task
    fn write_job_result_comment(
        &self,
        _task_id: &str,
        _step_id: &str,
        _work_item_id: &str,
        _job_succeeded: bool,
        _output: &serde_json::Value,
        _context: &serde_json::Value,
        _stored_comment_id: Option<i64>,
    ) -> anyhow::Result<Option<i64>> {
        Ok(None)
    }

    /// Read checkpoint from task (for recovery)
    fn read_checkpoint(&self, _task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support authoritative checkpoints",
            self.adapter_name()
        ))
    }

    /// Read triage result from task (for recovery)
    fn read_triage_result(&self, _task_id: &str) -> anyhow::Result<Option<TriageResult>> {
        Ok(None)
    }

    /// Set workflow/step/phase labels on the task
    fn set_labels(&self, _task_id: &str, _labels: &TaskLabels) -> anyhow::Result<()> {
        Ok(())
    }

    /// Set task assignee (bot or human)
    fn set_ownership(&self, _task_id: &str, _ownership: &ProviderOwnership) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support assignment mutations",
            self.adapter_name()
        ))
    }

    fn reject_action(
        &self,
        _task_id: &str,
        _rejection: ProviderActionRejectionDelivery<'_>,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support durable action rejection",
            self.adapter_name()
        ))
    }

    fn acknowledge_action(
        &self,
        _task_id: &str,
        _identity: &ProviderActionIdentity,
        _legacy_revision: Option<u64>,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "task adapter '{}' does not support durable action acknowledgement",
            self.adapter_name()
        ))
    }
}

pub fn work_item_from_create(
    req: &CreateWorkItemRequest,
    id: Uuid,
    workflow_id: String,
    current_step: String,
    adapter_name: &str,
) -> WorkItem {
    let now = Utc::now();
    let title = req
        .context
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let description = req
        .context
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    WorkItem {
        id,
        workflow_id,
        external_task_id: Some(id.to_string()),
        external_task_source: Some(adapter_name.to_string()),
        provider_ownership: ProviderOwnership::Bot,
        remembered_human_owners: vec![],
        title,
        description,
        labels: vec![],
        priority: None,
        session_id: req.session_id.clone(),
        current_step,
        projected_state: None,
        last_failure_reason: None,
        required_action_description: None,
        triage_status: None,
        human_gate: false,
        human_gate_state: None,
        blocked_on: None,
        state: WorkItemState::Pending,
        context: req.context.clone(),
        repo: req.repo.clone(),
        parent_work_item_id: None,
        parent_step_id: None,
        child_work_item_id: None,
        created_at: now,
        updated_at: now,
        last_written_at: None,
        write_back_failures: 0,
        write_back_retry_after: None,
        status_comment_id: None,
        checkpoint_comment_id: None,
        job_result_comment_ids: HashMap::new(),
    }
}

pub fn choose_workflow_id(task: &TaskToolRecord, workflow_ids: &[String]) -> Option<String> {
    let _ = workflow_ids;
    if let Some(explicit) = &task.workflow_id {
        return Some(explicit.clone());
    }

    task.context
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

pub fn task_from_work_item(item: &WorkItem, existing: Option<&TaskToolRecord>) -> TaskToolRecord {
    let inferred = infer_valid_next_events_for_projection(item);
    task_from_work_item_with_valid_next_events(item, existing, &inferred)
}

pub fn task_from_work_item_with_valid_next_events(
    item: &WorkItem,
    existing: Option<&TaskToolRecord>,
    valid_next_events: &[WorkflowEventKind],
) -> TaskToolRecord {
    let title = existing
        .map(|t| t.title.clone())
        .unwrap_or_else(|| format!("Task {}", item.id));
    let description = existing.and_then(|t| t.description.clone()).or_else(|| {
        item.context
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    });
    let action_revision = existing.map(|t| t.action_revision).unwrap_or(0);

    let is_triage = item.workflow_id == "__triage__";
    let title = item.title.clone().unwrap_or(title);
    let description = item.description.clone().or(description);
    let gate_state = current_human_gate_state(item);
    TaskToolRecord {
        id: item.id,
        ownership: item.provider_ownership.clone(),
        title,
        description,
        workflow_id: if is_triage {
            existing.and_then(|t| t.workflow_id.clone())
        } else {
            Some(item.workflow_id.clone())
        },
        triage_status: if is_triage {
            TaskTriageStatus::NeedsWorkflowSelection
        } else {
            triage_status_from_item(item)
        },
        pending_human_action: is_triage || is_human_gate(item),
        current_human_gate: if is_triage {
            Some("workflow_selection".to_string())
        } else {
            human_gate_label(item)
        },
        workflow_phase: Some(project_workflow_phase(item)),
        blocked_reason: project_blocked_reason(item),
        last_failure_reason: project_last_failure_reason(item),
        required_action_description: project_required_action_description(item, valid_next_events),
        human_gate_state: gate_state,
        valid_next_events: valid_next_events.to_vec(),
        allowed_actions: compute_allowed_actions(item, valid_next_events),
        context: item.context.clone(),
        repo: item.repo.clone(),
        updated_at: existing.map(|t| t.updated_at).unwrap_or(item.updated_at),
        requested_action: None,
        action_revision,
    }
}

fn current_human_gate_state(item: &WorkItem) -> Option<TaskHumanGateState> {
    if item.workflow_id != "__triage__" && !is_human_gate(item) {
        return None;
    }
    item.human_gate_state
        .as_deref()
        .and_then(parse_human_gate_state)
        .or_else(|| {
            item.context
                .get("task_human_gate_state")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
        })
}

fn parse_human_gate_state(v: &str) -> Option<TaskHumanGateState> {
    serde_json::from_value(serde_json::Value::String(v.to_string())).ok()
}

fn project_workflow_phase(item: &WorkItem) -> String {
    item.projected_state
        .clone()
        .unwrap_or_else(|| item.current_step.clone())
}

fn project_blocked_reason(item: &WorkItem) -> Option<String> {
    if item.workflow_id == "__triage__" {
        if let Some(reason) = item
            .context
            .get("triage_reason")
            .and_then(serde_json::Value::as_str)
        {
            return Some(reason.to_string());
        }
        return Some("workflow selection required".to_string());
    }
    if item.human_gate {
        if let Some(state) = &item.human_gate_state {
            return Some(format!("human gate: {}", state));
        }
        return Some("human gate awaiting action".to_string());
    }
    if item.state == WorkItemState::Failed {
        return Some("workflow step failed".to_string());
    }
    None
}

fn project_last_failure_reason(item: &WorkItem) -> Option<String> {
    if matches!(
        item.state,
        WorkItemState::Succeeded | WorkItemState::Cancelled
    ) {
        return None;
    }
    item.last_failure_reason.clone().or_else(|| {
        item.context
            .get("last_failure_reason")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn workflow_event_description(event: &WorkflowEventKind) -> &'static str {
    match event {
        WorkflowEventKind::Submit => "submit",
        WorkflowEventKind::HumanApproved => "approve",
        WorkflowEventKind::HumanChangesRequested => "request changes",
        WorkflowEventKind::HumanUnblocked => "unblock",
        WorkflowEventKind::RetryRequested => "retry",
        WorkflowEventKind::Cancel => "cancel",
        WorkflowEventKind::PrApproved => "PR approval",
        WorkflowEventKind::PrChangesRequested => "PR changes requested",
        WorkflowEventKind::OwnerPrApproved => "owner PR approval",
        WorkflowEventKind::OwnerChangesRequested => "owner changes requested",
        WorkflowEventKind::CoworkerPrApproved => "coworker PR approval",
        WorkflowEventKind::CoworkerChangesRequested => "coworker changes requested",
        WorkflowEventKind::PrMerged => "PR merge",
        WorkflowEventKind::ChecksPassed => "checks passed",
        WorkflowEventKind::ChecksFailed => "checks failed",
        _ => "operator action",
    }
}

fn project_required_action_description(
    item: &WorkItem,
    valid_next_events: &[WorkflowEventKind],
) -> Option<String> {
    if let Some(description) = &item.required_action_description {
        return Some(description.clone());
    }

    if item.workflow_id == "__triage__" {
        return Some("Needs triage: assign a workflow.".to_string());
    }

    if item.human_gate {
        let waiting_on = valid_next_events
            .iter()
            .filter(|event| **event != WorkflowEventKind::Cancel)
            .map(workflow_event_description)
            .collect::<Vec<_>>();
        if waiting_on.is_empty() {
            return Some("Waiting for operator action.".to_string());
        }
        return Some(format!("Waiting for: {}.", waiting_on.join(", ")));
    }

    if item.state == WorkItemState::Failed {
        return Some(format!("Failed at {}. Retry or cancel.", item.current_step));
    }

    if !valid_next_events.is_empty() {
        return Some("Waiting on automation.".to_string());
    }

    None
}

fn compute_allowed_actions(
    item: &WorkItem,
    valid_next_events: &[WorkflowEventKind],
) -> Vec<HumanActionKind> {
    if item.external_task_id.is_some() && !item.provider_ownership.is_bot_only() {
        return vec![];
    }
    if matches!(
        item.state,
        WorkItemState::Succeeded | WorkItemState::Cancelled
    ) {
        return vec![];
    }

    let allows = |event: WorkflowEventKind| valid_next_events.contains(&event);
    if item.workflow_id == "__triage__" {
        let mut actions = Vec::new();
        if item
            .human_gate_state
            .as_deref()
            .is_some_and(|state| state == "triage_completed")
        {
            if allows(WorkflowEventKind::HumanApproved) {
                actions.push(HumanActionKind::Approve);
            }
            if allows(WorkflowEventKind::HumanChangesRequested) {
                actions.push(HumanActionKind::RequestChanges);
            }
        }
        if item
            .human_gate_state
            .as_deref()
            .is_some_and(|s| s == "workflow_selected")
            && allows(WorkflowEventKind::Submit)
        {
            actions.push(HumanActionKind::Submit);
        }

        if item.current_step.as_str().eq("triage")
            && item.triage_status.as_deref() != Some("in_progress")
        {
            actions.push(HumanActionKind::ChooseWorkflow);
        }

        if allows(WorkflowEventKind::Cancel) {
            actions.push(HumanActionKind::Cancel);
        }
        return actions;
    }

    if item.human_gate {
        let mut actions = Vec::new();
        if allows(WorkflowEventKind::HumanApproved) {
            actions.push(HumanActionKind::Approve);
        }
        if allows(WorkflowEventKind::HumanChangesRequested) {
            actions.push(HumanActionKind::RequestChanges);
        }
        if allows(WorkflowEventKind::HumanUnblocked) {
            actions.push(HumanActionKind::Unblock);
        }
        if allows(WorkflowEventKind::RetryRequested) {
            actions.push(HumanActionKind::Retry);
        }
        if allows(WorkflowEventKind::Cancel) {
            actions.push(HumanActionKind::Cancel);
        }
        return actions;
    }

    match item.state {
        // Submit is currently a compatibility action and not shown by default.
        WorkItemState::Pending | WorkItemState::Running => {
            let mut actions = Vec::new();
            if allows(WorkflowEventKind::RetryRequested) {
                actions.push(HumanActionKind::Retry);
            }
            if allows(WorkflowEventKind::Cancel) {
                actions.push(HumanActionKind::Cancel);
            }
            actions
        }
        WorkItemState::Failed => {
            let mut actions = Vec::new();
            if allows(WorkflowEventKind::RetryRequested) {
                actions.push(HumanActionKind::Retry);
            }
            if allows(WorkflowEventKind::Cancel) {
                actions.push(HumanActionKind::Cancel);
            }
            actions
        }
        WorkItemState::Succeeded | WorkItemState::Cancelled => vec![],
    }
}

fn infer_valid_next_events_for_projection(item: &WorkItem) -> Vec<WorkflowEventKind> {
    // Compatibility fallback for contexts/tests that do not have orchestration-provided
    // valid_next_events yet. Keep this broadly aligned with the authoritative server helper,
    // but prefer orchestration::valid_next_events_for_work_item as the source of truth.
    if matches!(
        item.state,
        WorkItemState::Succeeded | WorkItemState::Cancelled
    ) {
        return vec![];
    }

    if item.workflow_id == "__triage__" {
        let mut events = Vec::new();
        if item
            .human_gate_state
            .as_deref()
            .is_some_and(|s| s == "workflow_selected")
        {
            events.push(WorkflowEventKind::Submit);
        }
        events.push(WorkflowEventKind::Cancel);
        return events;
    }

    if item.human_gate {
        return legacy_human_gate_valid_next_events(item);
    }

    let mut events = Vec::new();
    if item.state == WorkItemState::Failed {
        events.push(WorkflowEventKind::RetryRequested);
    }
    events.push(WorkflowEventKind::Cancel);
    events
}

pub(crate) fn legacy_human_gate_valid_next_events(item: &WorkItem) -> Vec<WorkflowEventKind> {
    let mut events = Vec::new();
    match current_human_gate_state(item) {
        Some(TaskHumanGateState::Approved) => {}
        Some(TaskHumanGateState::ChangesRequested) => {
            events.extend([
                WorkflowEventKind::HumanUnblocked,
                WorkflowEventKind::RetryRequested,
            ]);
        }
        Some(TaskHumanGateState::Unblocked) => {
            events.extend([
                WorkflowEventKind::HumanApproved,
                WorkflowEventKind::HumanChangesRequested,
            ]);
        }
        Some(TaskHumanGateState::RetryRequested) => {
            events.push(WorkflowEventKind::HumanUnblocked);
        }
        _ => {
            events.extend([
                WorkflowEventKind::HumanApproved,
                WorkflowEventKind::HumanChangesRequested,
                WorkflowEventKind::HumanUnblocked,
                WorkflowEventKind::RetryRequested,
            ]);
        }
    }
    events.push(WorkflowEventKind::Cancel);
    events
}

pub fn action_to_fire_event(action: &TaskHumanAction) -> Option<FireEventRequest> {
    let event = match action.kind {
        HumanActionKind::Submit => WorkflowEventKind::Submit,
        HumanActionKind::Approve => WorkflowEventKind::HumanApproved,
        HumanActionKind::RequestChanges => WorkflowEventKind::HumanChangesRequested,
        HumanActionKind::Unblock => WorkflowEventKind::HumanUnblocked,
        HumanActionKind::Retry => WorkflowEventKind::RetryRequested,
        HumanActionKind::Cancel => WorkflowEventKind::Cancel,
        HumanActionKind::ChecksPassed => WorkflowEventKind::ChecksPassed,
        HumanActionKind::ChecksFailed => WorkflowEventKind::ChecksFailed,
        HumanActionKind::ChooseWorkflow => return None,
    };
    Some(FireEventRequest {
        event,
        job_id: None,
        reason: action.reason.clone(),
    })
}

pub fn infer_intents(
    records: Vec<TaskSyncRecord>,
    existing: &HashMap<Uuid, WorkItem>,
    workflow_ids: &[String],
    snapshot_complete: bool,
    listed_task_ids: &[Uuid],
) -> Vec<AdapterIntent> {
    let write_back_grace = chrono::TimeDelta::seconds(5);
    let mut intents = Vec::new();
    let mut seen_tasks = listed_task_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    for record in records {
        let task = record.external;
        let ownership = record.ownership;
        let previous_ownership = record.previous_ownership;
        let ownership_revision = record.ownership_revision;
        seen_tasks.insert(task.id);
        if let Some(item) = existing.get(&task.id) {
            if ownership != item.provider_ownership {
                intents.push(AdapterIntent::OwnershipChanged {
                    work_item_id: item.id,
                    task: task.clone(),
                    ownership: ownership.clone(),
                    baseline_only: previous_ownership.is_none(),
                    ownership_revision,
                });
                if let Some(rejection) = record.provider_rejection {
                    intents.push(AdapterIntent::RejectAction {
                        work_item_id: Some(item.id),
                        task_id: task.id.to_string(),
                        action_revision: rejection.action_revision,
                        action_payload: rejection.action_payload,
                        reason: rejection.reason,
                        source: ProviderActionRejectionSource::Command,
                    });
                } else if ownership.is_bot_only()
                    && let Some(action) = &task.requested_action
                {
                    intents.push(AdapterIntent::ApplyAction {
                        work_item_id: item.id,
                        action: action.clone(),
                        action_revision: task.action_revision,
                    });
                }
                continue;
            }
            if let Some(rejection) = record.provider_rejection {
                intents.push(AdapterIntent::RejectAction {
                    work_item_id: Some(item.id),
                    task_id: task.id.to_string(),
                    action_revision: rejection.action_revision,
                    action_payload: rejection.action_payload,
                    reason: rejection.reason,
                    source: ProviderActionRejectionSource::Command,
                });
                continue;
            }
            if !ownership.is_bot_only() {
                continue;
            }
            if let Some(action) = &task.requested_action {
                // requested_action is a one-shot intent; dedupe is enforced by action_revision
                // against context.task_adapter.last_action_revision on the orchestration side.
                intents.push(AdapterIntent::ApplyAction {
                    work_item_id: item.id,
                    action: action.clone(),
                    action_revision: task.action_revision,
                });
            } else {
                if task.updated_at <= item.updated_at {
                    continue;
                }
                if item.last_written_at.is_some_and(|last_written_at| {
                    (task.updated_at - last_written_at).num_milliseconds().abs()
                        <= write_back_grace.num_milliseconds()
                }) {
                    continue;
                }
                if item
                    .write_back_retry_after
                    .is_some_and(|retry_after| retry_after > Utc::now())
                {
                    continue;
                }
                intents.push(AdapterIntent::UpdateProjection {
                    work_item_id: item.id,
                    task,
                });
            }
            continue;
        }

        if matches!(ownership, ProviderOwnership::BotAndHuman { .. }) {
            intents.push(AdapterIntent::RejectAction {
                work_item_id: None,
                task_id: task.id.to_string(),
                action_revision: ownership_revision,
                action_payload: serde_json::to_string(&ownership)
                    .expect("provider ownership serializes"),
                reason: "bot and human assignment cannot coexist".to_string(),
                source: ProviderActionRejectionSource::Ownership {
                    ownership: ownership.clone(),
                    intent: "illegal_assignment",
                },
            });
            continue;
        }
        if !ownership.is_bot_only() {
            continue;
        }
        if let Some(pin) = choose_workflow_id(&task, workflow_ids)
            && !workflow_ids.iter().any(|workflow_id| workflow_id == &pin)
        {
            intents.push(AdapterIntent::RejectAction {
                work_item_id: None,
                task_id: task.id.to_string(),
                action_revision: ownership_revision,
                action_payload: pin.clone(),
                reason: format!("unknown explicit workflow pin '{pin}'"),
                source: ProviderActionRejectionSource::Ownership {
                    ownership: ownership.clone(),
                    intent: "illegal_workflow_pin",
                },
            });
            continue;
        }
        intents.push(AdapterIntent::Create {
            task,
            ownership,
            previous_ownership,
        });
    }

    if !snapshot_complete {
        return intents;
    }
    for item in existing.values() {
        if matches!(
            item.state,
            WorkItemState::Succeeded | WorkItemState::Cancelled
        ) {
            continue;
        }
        let Some(task_id) = item.external_task_id.as_deref() else {
            continue;
        };
        let Ok(task_uuid) = Uuid::parse_str(task_id) else {
            continue;
        };
        if seen_tasks.contains(&task_uuid) {
            continue;
        }
        intents.push(AdapterIntent::CancelMissingExternalTask {
            work_item_id: item.id,
            task_id: task_id.to_string(),
        });
    }

    intents
}

fn triage_status_from_item(item: &WorkItem) -> TaskTriageStatus {
    if item.workflow_id == "__triage__" {
        return TaskTriageStatus::NeedsWorkflowSelection;
    }
    match item.state {
        WorkItemState::Pending => TaskTriageStatus::Ready,
        WorkItemState::Running => {
            if is_human_gate(item) {
                TaskTriageStatus::Blocked
            } else {
                TaskTriageStatus::InProgress
            }
        }
        WorkItemState::Succeeded => TaskTriageStatus::Completed,
        WorkItemState::Failed => TaskTriageStatus::Blocked,
        WorkItemState::Cancelled => TaskTriageStatus::Cancelled,
    }
}

fn is_human_gate(item: &WorkItem) -> bool {
    item.human_gate
        || item
            .context
            .get("human_gate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn human_gate_label(item: &WorkItem) -> Option<String> {
    if is_human_gate(item) {
        Some(item.current_step.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_test_task() -> TaskToolRecord {
        TaskToolRecord {
            id: Uuid::new_v4(),
            ownership: ProviderOwnership::Bot,
            title: "task".into(),
            description: None,
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        }
    }

    #[test]
    fn checkpoint_deserializes_pre_additive_fixture() {
        let checkpoint: TaskCheckpoint = serde_json::from_value(serde_json::json!({
            "current_step": "build",
            "phase": "executing",
            "workflow_id": "wf",
            "repo": null,
            "pr_url": null,
            "pr_status": null,
            "human_gate": false,
            "failure_info": null,
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .expect("pre-additive checkpoint fixture");
        assert!(checkpoint.human_gate_state.is_none());
        assert!(checkpoint.triage_status.is_none());
        assert!(checkpoint.human_owner.is_none());
        assert!(checkpoint.human_owners.is_empty());
    }

    #[test]
    fn unassigned_task_is_not_ingress() {
        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: sync_test_task(),
                ownership: ProviderOwnership::Unassigned,
                previous_ownership: None,
                ownership_revision: 1,
                provider_rejection: None,
            }],
            &HashMap::new(),
            &[],
            true,
            &[],
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn mixed_bot_and_human_assignment_is_rejected() {
        let task = sync_test_task();
        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task,
                ownership: ProviderOwnership::BotAndHuman {
                    owners: vec![protocol::ProviderIdentity {
                        provider_id: Some("42".to_string()),
                        display_name: Some("alice".to_string()),
                    }],
                },
                previous_ownership: None,
                ownership_revision: 1,
                provider_rejection: None,
            }],
            &HashMap::new(),
            &[],
            true,
            &[],
        );
        assert!(matches!(
            intents.as_slice(),
            [AdapterIntent::RejectAction { .. }]
        ));
    }

    #[test]
    fn invalid_command_does_not_outrank_deassignment() {
        let task = sync_test_task();
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".to_string(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            task.id,
            "wf".to_string(),
            "build".to_string(),
            "test",
        );
        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task.clone(),
                ownership: ProviderOwnership::Unassigned,
                previous_ownership: Some(ProviderOwnership::Bot),
                ownership_revision: 7,
                provider_rejection: Some(TaskProviderRejection {
                    action_revision: 8,
                    action_payload: "/ah unknown".to_string(),
                    reason: "unknown /ah command 'unknown'".to_string(),
                }),
            }],
            &HashMap::from([(task.id, item)]),
            &[],
            true,
            &[],
        );
        assert!(matches!(
            intents.as_slice(),
            [
                AdapterIntent::OwnershipChanged {
                    ownership: ProviderOwnership::Unassigned,
                    ..
                },
                AdapterIntent::RejectAction { .. }
            ]
        ));
    }

    #[test]
    fn invalid_command_does_not_outrank_mixed_assignment() {
        let task = sync_test_task();
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".to_string(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            task.id,
            "wf".to_string(),
            "build".to_string(),
            "test",
        );
        let ownership = ProviderOwnership::BotAndHuman {
            owners: vec![protocol::ProviderIdentity {
                provider_id: Some("42".to_string()),
                display_name: Some("alice".to_string()),
            }],
        };
        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task.clone(),
                ownership: ownership.clone(),
                previous_ownership: Some(ProviderOwnership::Bot),
                ownership_revision: 7,
                provider_rejection: Some(TaskProviderRejection {
                    action_revision: 8,
                    action_payload: "/ah unknown".to_string(),
                    reason: "unknown /ah command 'unknown'".to_string(),
                }),
            }],
            &HashMap::from([(task.id, item)]),
            &[],
            true,
            &[],
        );
        assert!(matches!(
            intents.as_slice(),
            [AdapterIntent::OwnershipChanged { ownership: actual, .. }, AdapterIntent::RejectAction { .. }]
                if actual == &ownership
        ));
    }

    #[test]
    fn incomplete_snapshot_never_infers_deletion() {
        let task_id = Uuid::new_v4();
        let request = CreateWorkItemRequest {
            workflow_id: "wf".to_string(),
            session_id: None,
            context: serde_json::json!({}),
            repo: None,
        };
        let item = work_item_from_create(
            &request,
            task_id,
            "wf".to_string(),
            "build".to_string(),
            "file",
        );
        let intents = infer_intents(
            vec![],
            &HashMap::from([(task_id, item)]),
            &["wf".to_string()],
            false,
            &[],
        );
        assert!(intents.is_empty());
    }

    #[test]
    fn unknown_explicit_workflow_pin_is_rejected_not_dropped() {
        let mut task = sync_test_task();
        task.workflow_id = Some("removed_workflow".to_string());
        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task,
                ownership: ProviderOwnership::Bot,
                previous_ownership: Some(ProviderOwnership::Unassigned),
                ownership_revision: 1,
                provider_rejection: None,
            }],
            &HashMap::new(),
            &["wf".to_string()],
            true,
            &[],
        );
        assert!(matches!(
            intents.as_slice(),
            [AdapterIntent::RejectAction { reason, .. }]
                if reason.contains("unknown explicit workflow pin 'removed_workflow'")
        ));
    }

    #[test]
    fn choose_workflow_prefers_explicit() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            ownership: ProviderOwnership::Bot,
            title: "t".into(),
            description: None,
            workflow_id: Some("wf1".into()),
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"workflow_id": "wf2"}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        assert_eq!(
            choose_workflow_id(&task, &["wf1".into(), "wf2".into()]),
            Some("wf1".into())
        );
    }

    #[test]
    fn infer_intents_marks_missing_workflow_for_triage() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            ownership: ProviderOwnership::Bot,
            title: "t".into(),
            description: None,
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };

        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task.clone(),
                ownership: ProviderOwnership::Bot,
                previous_ownership: None,
                ownership_revision: 1,
                provider_rejection: None,
            }],
            &HashMap::new(),
            &[],
            true,
            &[],
        );
        assert!(matches!(intents[0], AdapterIntent::Create { .. }));
    }

    #[test]
    fn task_projection_prefers_work_item_human_gate_field() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({"human_gate": false}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
            "file",
        );
        item.human_gate = true;
        let task = task_from_work_item(&item, None);
        assert!(task.pending_human_action);
    }

    #[test]
    fn allowed_actions_for_triage_and_human_gate() {
        let mut triage = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
            "file",
        );
        triage.workflow_id = "__triage__".into();
        let triage_task = task_from_work_item(&triage, None);
        assert!(
            triage_task
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let mut human = triage.clone();
        human.workflow_id = "wf".into();
        human.human_gate = true;
        let human_task = task_from_work_item(&human, None);
        assert!(
            human_task
                .allowed_actions
                .contains(&HumanActionKind::Approve)
        );
        assert!(
            human_task
                .allowed_actions
                .contains(&HumanActionKind::RequestChanges)
        );

        human.human_gate_state = Some("awaiting_human_action".into());
        let awaiting = task_from_work_item(&human, None);
        assert_eq!(
            awaiting.human_gate_state,
            Some(TaskHumanGateState::AwaitingHumanAction)
        );
        assert!(awaiting.allowed_actions.contains(&HumanActionKind::Approve));

        human.human_gate_state = Some("changes_requested".into());
        let narrowed = task_from_work_item(&human, None);
        assert!(!narrowed.allowed_actions.contains(&HumanActionKind::Approve));
        assert!(narrowed.allowed_actions.contains(&HumanActionKind::Unblock));
        assert!(narrowed.allowed_actions.contains(&HumanActionKind::Retry));
    }

    #[test]
    fn pending_non_triage_does_not_offer_submit_by_default() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "build".into(),
            "file",
        );
        let task = task_from_work_item(&item, None);
        assert!(!task.allowed_actions.contains(&HumanActionKind::Submit));
    }

    #[test]
    fn needs_triage_flag_is_informational() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({"needs_triage": true, "triage_reason": "manual triage required"}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
            "file",
        );
        item.workflow_id = "__triage__".into();
        let task = task_from_work_item(&item, None);
        assert!(
            task.allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );
        assert_eq!(
            task.blocked_reason.as_deref(),
            Some("manual triage required")
        );
    }

    #[test]
    fn needs_triage_label_is_informational() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({"labels": ["needs-triage"], "triage_reason": "manual triage required"}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
            "file",
        );
        item.workflow_id = "__triage__".into();
        item.labels = vec!["needs-triage".to_string()];

        let task = task_from_work_item(&item, None);
        assert!(
            task.allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        item.labels.clear();
        item.context = serde_json::json!({"labels": []});
        let unblocked = task_from_work_item(&item, None);
        assert!(
            unblocked
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );
    }

    #[test]
    fn infer_intents_respects_needs_triage_label() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            ownership: ProviderOwnership::Bot,
            title: "t".into(),
            description: None,
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            last_failure_reason: None,
            required_action_description: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"labels": ["needs-triage"]}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };

        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task,
                ownership: ProviderOwnership::Bot,
                previous_ownership: None,
                ownership_revision: 1,
                provider_rejection: None,
            }],
            &HashMap::new(),
            &["wf".into()],
            true,
            &[],
        );
        assert!(matches!(intents[0], AdapterIntent::Create { .. }));
    }

    #[test]
    fn allowed_actions_are_subset_of_valid_next_events() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
            "file",
        );

        let task = task_from_work_item_with_valid_next_events(
            &item,
            None,
            &[WorkflowEventKind::Cancel, WorkflowEventKind::RetryRequested],
        );

        for action in &task.allowed_actions {
            let mapped = action_to_fire_event(&TaskHumanAction {
                kind: action.clone(),
                workflow_id: None,
                reason: None,
            })
            .map(|req| req.event);
            if let Some(event) = mapped {
                assert!(task.valid_next_events.contains(&event));
            }
        }
    }

    #[test]
    fn fallback_valid_next_events_for_human_gate_are_state_aware() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
            "file",
        );
        item.human_gate = true;
        item.human_gate_state = Some("changes_requested".into());

        let task = task_from_work_item(&item, None);
        assert!(
            task.valid_next_events
                .contains(&WorkflowEventKind::HumanUnblocked)
        );
        assert!(
            task.valid_next_events
                .contains(&WorkflowEventKind::RetryRequested)
        );
        assert!(task.valid_next_events.contains(&WorkflowEventKind::Cancel));
        assert!(
            !task
                .valid_next_events
                .contains(&WorkflowEventKind::HumanApproved)
        );
        assert!(
            !task
                .valid_next_events
                .contains(&WorkflowEventKind::HumanChangesRequested)
        );
    }

    #[test]
    fn projection_hides_human_gate_state_when_not_in_human_gate() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({"task_human_gate_state": "changes_requested"}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "run".into(),
            "file",
        );
        item.human_gate = false;
        item.human_gate_state = Some("changes_requested".into());

        let task = task_from_work_item(&item, None);
        assert_eq!(task.human_gate_state, None);
    }

    #[test]
    fn projection_hides_failure_reason_for_succeeded_or_cancelled_items() {
        let mut succeeded = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({"last_failure_reason": "stale"}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "done".into(),
            "file",
        );
        succeeded.state = WorkItemState::Succeeded;
        succeeded.last_failure_reason = Some("stale".into());
        assert_eq!(
            task_from_work_item(&succeeded, None).last_failure_reason,
            None
        );

        let mut cancelled = succeeded;
        cancelled.state = WorkItemState::Cancelled;
        assert_eq!(
            task_from_work_item(&cancelled, None).last_failure_reason,
            None
        );
    }

    #[test]
    fn running_items_offer_retry_when_valid_next_events_include_retry_requested() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "run".into(),
            "file",
        );
        item.state = WorkItemState::Running;

        let task = task_from_work_item_with_valid_next_events(
            &item,
            None,
            &[WorkflowEventKind::RetryRequested, WorkflowEventKind::Cancel],
        );

        assert!(task.allowed_actions.contains(&HumanActionKind::Retry));
        assert!(task.allowed_actions.contains(&HumanActionKind::Cancel));
    }

    #[test]
    fn work_item_from_create_lifts_title_and_description_from_context() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({
                    "title": "Context title",
                    "description": "Context description"
                }),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "build".into(),
            "file",
        );

        assert_eq!(item.title.as_deref(), Some("Context title"));
        assert_eq!(item.description.as_deref(), Some("Context description"));
    }
}

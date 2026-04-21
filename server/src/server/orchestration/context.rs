pub(super) const CONTEXT_JOB_OUTPUTS_KEY: &str = "job_outputs";
pub(super) const CONTEXT_JOB_RESULTS_KEY: &str = "job_results";
pub(super) const CONTEXT_JOB_ARTIFACT_PATHS_KEY: &str = "job_artifact_paths";
pub(super) const CONTEXT_JOB_ARTIFACT_METADATA_KEY: &str = "job_artifact_metadata";
pub(super) const CONTEXT_JOB_ARTIFACT_DATA_KEY: &str = "job_artifact_data";
pub(super) const CONTEXT_WORKFLOW_KEY: &str = "workflow";
pub(super) const CONTEXT_WORKFLOW_INPUTS_KEY: &str = "inputs";
pub(super) const CONTEXT_VALID_NEXT_EVENTS_KEY: &str = "valid_next_events";
pub(super) const CONTEXT_WORK_ITEM_KEY: &str = "work_item";
pub(super) const CONTEXT_JOB_EXECUTION_KIND_KEY: &str = "job_execution_kind";
pub(super) const CONTEXT_MANAGED_OPERATION_KEY: &str = "managed_operation";
pub(super) const CONTEXT_REPO_CATALOG_KEY: &str = "repo_catalog";
pub(super) const CONTEXT_REPO_CATALOG_HINT_KEY: &str = "repo_catalog_hint";
/// Pseudo-workflow id used as the universal triage ingress route. See
/// `agent-hub-configs/triage.toml` for the prompt this drives, and
/// `reactive-agents-north-star.md` § "Triage / Workflow Selection".
pub(super) const TRIAGE_WORKFLOW_ID: &str = "__triage__";
pub(crate) const CONTEXT_GITHUB_PR_STATE_KEY: &str = "github_pr_state";

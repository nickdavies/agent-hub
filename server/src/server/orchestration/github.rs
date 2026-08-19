use chrono::{DateTime, Utc};
use protocol::{MergePolicy, WorkflowEventKind};
use serde::{Deserialize, Serialize};

use super::CONTEXT_GITHUB_PR_STATE_KEY;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GithubPrLifecycleState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GithubPrState {
    pub url: String,
    pub number: u64,
    #[serde(default)]
    pub head_branch: Option<String>,
    pub state: GithubPrLifecycleState,
    #[serde(default)]
    pub approval_count: u32,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub passing_checks: Vec<String>,
    #[serde(default)]
    pub checks_passing: bool,
    #[serde(default)]
    pub changes_requested: bool,
    #[serde(default)]
    pub merge_policy: Option<MergePolicy>,
    #[serde(default)]
    pub merge_attempted: bool,
    pub mergeable: bool,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergeReadiness {
    pub ready: bool,
    pub is_open: bool,
    pub approved: bool,
    pub checks_passing: bool,
    pub no_changes_requested: bool,
}

pub(super) fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    Some(protocol::parse_github_pull_request_url(url)?.number)
}

/// Components extracted from a GitHub-style PR URL, including the host.
///
/// Used by the orchestrator when promoting `github_pr` from a job's
/// structured output into the work-item context, so the SCM host
/// registry / poller knows which API endpoint to talk to.
pub(super) struct PrUrlParts {
    pub host: String,
    pub owner: String,
    pub repo: String,
    #[allow(dead_code)]
    pub number: Option<u64>,
}

/// Parse `https://<host>/<owner>/<repo>/pull/<n>` (with or without `.git`
/// on the repo segment) into its
/// components. Accepts arbitrary hosts so it works for both `github.com`
/// and GitHub Enterprise instances such as `github.snooguts.net`.
///
/// Rejects API-style URLs (`api.github.com/repos/...`) since those are
/// not what humans paste into a task description and `gh pr create` does
/// not produce them. Rejects anything that isn't `.../pull/<id>`.
pub(super) fn parse_pr_url_components(url: &str) -> Option<PrUrlParts> {
    let parsed = protocol::parse_github_pull_request_url(url)?;
    Some(PrUrlParts {
        host: parsed.repository.host,
        owner: parsed.repository.owner,
        repo: parsed.repository.repository,
        number: Some(parsed.number),
    })
}

pub(super) fn github_pr_state_from_context(context: &serde_json::Value) -> Option<GithubPrState> {
    context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value::<GithubPrState>(value).ok())
}

fn github_pr_state_default(url: String, number: u64, now: DateTime<Utc>) -> GithubPrState {
    GithubPrState {
        url,
        number,
        head_branch: None,
        state: GithubPrLifecycleState::Open,
        approval_count: 0,
        approved: false,
        passing_checks: Vec::new(),
        checks_passing: false,
        changes_requested: false,
        merge_policy: None,
        merge_attempted: false,
        mergeable: false,
        last_updated: now,
    }
}

pub(super) fn merge_readiness_from_pr_state(pr_state: &GithubPrState) -> MergeReadiness {
    let is_open = matches!(pr_state.state, GithubPrLifecycleState::Open);
    let (approved, checks_passing) = if let Some(policy) = pr_state.merge_policy.as_ref() {
        let approved = pr_state.approval_count >= policy.required_approvals;
        let checks_passing = policy.required_checks.iter().all(|required| {
            pr_state
                .passing_checks
                .iter()
                .any(|passing| passing == required)
        });
        (approved, checks_passing)
    } else {
        (pr_state.approved, pr_state.checks_passing)
    };
    let no_changes_requested = !pr_state.changes_requested;
    let ready = is_open && approved && checks_passing && no_changes_requested;

    MergeReadiness {
        ready,
        is_open,
        approved,
        checks_passing,
        no_changes_requested,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_github_pr_state_in_context(
    context: &mut serde_json::Value,
    pr_url: &str,
    pr_number: Option<u64>,
    head_branch: Option<&str>,
    event: Option<&WorkflowEventKind>,
    check_name: Option<&str>,
    pr_closed_unmerged: bool,
    now: DateTime<Utc>,
) {
    let default_number = pr_number
        .or_else(|| parse_pr_number_from_url(pr_url))
        .unwrap_or(0);
    let mut pr_state = github_pr_state_from_context(context)
        .unwrap_or_else(|| github_pr_state_default(pr_url.to_string(), default_number, now));

    pr_state.url = pr_url.to_string();
    if let Some(number) = pr_number.or_else(|| parse_pr_number_from_url(pr_url)) {
        pr_state.number = number;
    }
    if let Some(branch) = head_branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        pr_state.head_branch = Some(branch.to_string());
    }

    match event {
        Some(WorkflowEventKind::PrCreated) => {
            pr_state.state = GithubPrLifecycleState::Open;
            pr_state.approval_count = 0;
            pr_state.approved = false;
            pr_state.passing_checks.clear();
            pr_state.checks_passing = false;
            pr_state.changes_requested = false;
            pr_state.merge_attempted = false;
        }
        Some(
            WorkflowEventKind::PrApproved
            | WorkflowEventKind::OwnerPrApproved
            | WorkflowEventKind::CoworkerPrApproved,
        ) => {
            pr_state.approval_count = pr_state.approval_count.saturating_add(1);
            pr_state.approved = true;
            pr_state.changes_requested = false;
        }
        Some(
            WorkflowEventKind::HumanChangesRequested
            | WorkflowEventKind::PrChangesRequested
            | WorkflowEventKind::OwnerChangesRequested
            | WorkflowEventKind::CoworkerChangesRequested,
        ) => {
            pr_state.changes_requested = true;
            pr_state.approval_count = 0;
            pr_state.approved = false;
        }
        Some(WorkflowEventKind::ChecksPassed) => {
            pr_state.checks_passing = true;
            if let Some(name) = check_name {
                let name = name.trim();
                if !name.is_empty()
                    && !pr_state
                        .passing_checks
                        .iter()
                        .any(|existing| existing == name)
                {
                    pr_state.passing_checks.push(name.to_string());
                }
            }
        }
        Some(WorkflowEventKind::ChecksFailed) => {
            pr_state.checks_passing = false;
            if let Some(name) = check_name {
                pr_state
                    .passing_checks
                    .retain(|existing| existing != name.trim());
            }
        }
        Some(WorkflowEventKind::PrMerged) => {
            pr_state.state = GithubPrLifecycleState::Merged;
            pr_state.merge_attempted = true;
        }
        _ => {}
    }
    if pr_closed_unmerged {
        pr_state.state = GithubPrLifecycleState::Closed;
    }

    let readiness = merge_readiness_from_pr_state(&pr_state);
    pr_state.mergeable = readiness.ready;
    pr_state.last_updated = now;

    if !context.is_object() {
        *context = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(ctx_obj) = context.as_object_mut()
        && let Ok(value) = serde_json::to_value(pr_state)
    {
        ctx_obj.insert(CONTEXT_GITHUB_PR_STATE_KEY.to_string(), value);
    }
}

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use protocol::{WorkItem, WorkflowEventKind};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use uuid::Uuid;

use super::scm::{ResolvedScmHost, ScmHostKind, ScmHostRegistry};

/// One unit of work the poller hands back to the orchestration loop:
/// a workflow event to fire on a work item, plus an optional patch to
/// merge into the work item's context *before* the event fires.
///
/// Today the only patch we emit is `last_ci_failure` on `ChecksFailed`
/// (so the future `triage_ci_failure` agent step has the failed run
/// names + URLs available without re-querying GitHub). The shape is
/// generic so other event kinds can attach context without churning
/// the call sites.
#[derive(Debug, Clone)]
pub struct PollerEvent {
    pub work_item_id: Uuid,
    pub event: WorkflowEventKind,
    pub context_patch: Option<JsonMap<String, JsonValue>>,
}

impl PollerEvent {
    fn bare(work_item_id: Uuid, event: WorkflowEventKind) -> Self {
        Self {
            work_item_id,
            event,
            context_patch: None,
        }
    }

    fn with_patch(
        work_item_id: Uuid,
        event: WorkflowEventKind,
        patch: JsonMap<String, JsonValue>,
    ) -> Self {
        Self {
            work_item_id,
            event,
            context_patch: Some(patch),
        }
    }
}

/// Polls GitHub-flavoured hosts for PR / check / review state changes
/// and translates those into workflow events.
///
/// Multi-host: the host comes from each work item's `context.github_pr.host`
/// field. We look the host up in the `ScmHostRegistry` to get the right
/// `api_base_url` + token. This means one `GitHubPoller` instance handles
/// `github.com`, `github.snooguts.net`, and any other GitHub-kind hosts in
/// the registry uniformly.
///
/// Per-PR poll state is tracked under a host-aware key so the same
/// `(owner, repo, number)` on different GitHub instances doesn't collide.
pub struct GitHubPoller {
    client: reqwest::Client,
    registry: ScmHostRegistry,
    known_state: Mutex<HashMap<String, PrKnownState>>,
}

/// What the poller has observed for a PR most recently. `head_sha` and
/// `last_change_at` together drive the `await_ci` timeout reset semantics:
/// a new push (head_sha changes) resets the "stuck pending" clock, while
/// transient poll failures or repeated identical responses do not.
#[derive(Debug, Clone, Default)]
pub struct PrKnownState {
    pub head_sha: Option<String>,
    check_status: Option<CheckStatus>,
    review_state: Vec<ReviewInfo>,
    merged: bool,
    closed: bool,
    pub last_change_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Pending,
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewInfo {
    reviewer: String,
    state: ReviewState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewState {
    Approved,
    ChangesRequested,
    Other,
}

#[derive(Debug)]
enum PollError {
    RateLimited,
    Other(anyhow::Error),
}

impl GitHubPoller {
    pub fn new(registry: ScmHostRegistry) -> Self {
        Self {
            client: reqwest::Client::new(),
            registry,
            known_state: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot the poller's known PR state for a given work item, by
    /// looking up the work item's `github_pr` context. Used by the
    /// orchestration loop's timeout-escalation logic so it can decide
    /// whether `await_ci` has been stuck-pending past its threshold.
    pub fn known_state_for_item(&self, item: &WorkItem) -> Option<PrKnownState> {
        let pr_ctx = github_pr_context(item)?;
        let key = pr_key(&pr_ctx.host, &pr_ctx.owner, &pr_ctx.repo, pr_ctx.number);
        self.known_state
            .lock()
            .ok()
            .and_then(|states| states.get(&key).cloned())
    }

    pub async fn poll_work_items(
        &self,
        work_items: &[WorkItem],
    ) -> anyhow::Result<Vec<PollerEvent>> {
        let mut out = Vec::new();

        for item in work_items {
            let Some(pr_ctx) = github_pr_context(item) else {
                continue;
            };
            // Hosts in the registry are guaranteed to have a non-empty
            // token (see ScmHostRegistry::from_configs). A miss here
            // means the work item references a host the operator never
            // declared — that's an operator error, surfaced loudly so
            // it doesn't hide as a silently-stalled PR.
            let Some(host_cfg) = self.registry.find(&pr_ctx.host) else {
                tracing::warn!(
                    work_item_id = %item.id,
                    host = %pr_ctx.host,
                    "github poller: work item references host '{}' which is not registered in [[system.scm_hosts]]; \
                     skipping (add the host or use /ah checks_passed/checks_failed to advance manually)",
                    pr_ctx.host,
                );
                continue;
            };
            if !matches!(host_cfg.kind, ScmHostKind::Github) {
                continue;
            }
            let key = pr_key(&pr_ctx.host, &pr_ctx.owner, &pr_ctx.repo, pr_ctx.number);
            let previous = self
                .known_state
                .lock()
                .ok()
                .and_then(|states| states.get(&key).cloned())
                .unwrap_or_default();

            let pr = match self
                .get_json::<PullRequestResponse>(
                    host_cfg,
                    &format!(
                        "/repos/{}/{}/pulls/{}",
                        pr_ctx.owner, pr_ctx.repo, pr_ctx.number
                    ),
                )
                .await
            {
                Ok(Some(pr)) => pr,
                Ok(None) => continue,
                Err(PollError::RateLimited) => {
                    tracing::warn!("github API returned 403/429; skipping poll tick");
                    return Ok(Vec::new());
                }
                Err(PollError::Other(error)) => {
                    tracing::warn!(
                        work_item_id = %item.id,
                        owner = %pr_ctx.owner,
                        repo = %pr_ctx.repo,
                        pr_number = pr_ctx.number,
                        "failed to fetch pull request state: {error:#}"
                    );
                    continue;
                }
            };

            let check_runs = match self
                .get_json::<CheckRunsResponse>(
                    host_cfg,
                    &format!(
                        "/repos/{}/{}/commits/{}/check-runs",
                        pr_ctx.owner, pr_ctx.repo, pr.head.sha
                    ),
                )
                .await
            {
                Ok(Some(value)) => value,
                Ok(None) => CheckRunsResponse::default(),
                Err(PollError::RateLimited) => {
                    tracing::warn!("github API returned 403/429; skipping poll tick");
                    return Ok(Vec::new());
                }
                Err(PollError::Other(error)) => {
                    tracing::warn!(
                        work_item_id = %item.id,
                        owner = %pr_ctx.owner,
                        repo = %pr_ctx.repo,
                        pr_number = pr_ctx.number,
                        "failed to fetch check runs: {error:#}"
                    );
                    continue;
                }
            };

            let status = match self
                .get_json::<CommitStatusResponse>(
                    host_cfg,
                    &format!(
                        "/repos/{}/{}/commits/{}/status",
                        pr_ctx.owner, pr_ctx.repo, pr.head.sha
                    ),
                )
                .await
            {
                Ok(Some(value)) => value,
                Ok(None) => CommitStatusResponse::default(),
                Err(PollError::RateLimited) => {
                    tracing::warn!("github API returned 403/429; skipping poll tick");
                    return Ok(Vec::new());
                }
                Err(PollError::Other(error)) => {
                    tracing::warn!(
                        work_item_id = %item.id,
                        owner = %pr_ctx.owner,
                        repo = %pr_ctx.repo,
                        pr_number = pr_ctx.number,
                        "failed to fetch commit status: {error:#}"
                    );
                    continue;
                }
            };

            let reviews = match self
                .get_json::<Vec<ReviewResponse>>(
                    host_cfg,
                    &format!(
                        "/repos/{}/{}/pulls/{}/reviews",
                        pr_ctx.owner, pr_ctx.repo, pr_ctx.number
                    ),
                )
                .await
            {
                Ok(Some(value)) => value,
                Ok(None) => Vec::new(),
                Err(PollError::RateLimited) => {
                    tracing::warn!("github API returned 403/429; skipping poll tick");
                    return Ok(Vec::new());
                }
                Err(PollError::Other(error)) => {
                    tracing::warn!(
                        work_item_id = %item.id,
                        owner = %pr_ctx.owner,
                        repo = %pr_ctx.repo,
                        pr_number = pr_ctx.number,
                        "failed to fetch pull request reviews: {error:#}"
                    );
                    continue;
                }
            };

            let mut item_events: Vec<PollerEvent> = Vec::new();

            if pr.merged && !previous.merged {
                push_event_unique(
                    &mut item_events,
                    PollerEvent::bare(item.id, WorkflowEventKind::PrMerged),
                );
            }
            if pr.state.eq_ignore_ascii_case("closed") && !previous.closed {
                tracing::debug!(
                    work_item_id = %item.id,
                    owner = %pr_ctx.owner,
                    repo = %pr_ctx.repo,
                    pr_number = pr_ctx.number,
                    "pull request closed; no matching workflow event configured"
                );
            }

            let check_status = Some(compute_check_status(&check_runs, &status));
            if matches!(check_status, Some(CheckStatus::Success))
                && !matches!(previous.check_status, Some(CheckStatus::Success))
            {
                push_event_unique(
                    &mut item_events,
                    PollerEvent::bare(item.id, WorkflowEventKind::ChecksPassed),
                );
            }
            if matches!(check_status, Some(CheckStatus::Failure))
                && !matches!(previous.check_status, Some(CheckStatus::Failure))
            {
                let patch = build_ci_failure_context_patch(
                    &pr_ctx.host,
                    &pr.head.sha,
                    &check_runs,
                    &status,
                );
                push_event_unique(
                    &mut item_events,
                    PollerEvent::with_patch(item.id, WorkflowEventKind::ChecksFailed, patch),
                );
            }

            let latest_reviews = latest_reviews_by_reviewer(reviews);
            let previous_reviews = previous
                .review_state
                .iter()
                .map(|review| (review.reviewer.as_str(), review.state.clone()))
                .collect::<HashMap<_, _>>();
            for review in &latest_reviews {
                let old_state = previous_reviews.get(review.reviewer.as_str());
                if old_state == Some(&review.state) {
                    continue;
                }
                let is_owner = pr_ctx
                    .owner_login
                    .as_deref()
                    .is_some_and(|owner| owner.eq_ignore_ascii_case(&review.reviewer));
                match review.state {
                    ReviewState::Approved => {
                        let role_event = if is_owner {
                            WorkflowEventKind::OwnerPrApproved
                        } else {
                            WorkflowEventKind::CoworkerPrApproved
                        };
                        push_event_unique(
                            &mut item_events,
                            PollerEvent::bare(item.id, role_event),
                        );
                        push_event_unique(
                            &mut item_events,
                            PollerEvent::bare(item.id, WorkflowEventKind::PrApproved),
                        );
                    }
                    ReviewState::ChangesRequested => {
                        let role_event = if is_owner {
                            WorkflowEventKind::OwnerChangesRequested
                        } else {
                            WorkflowEventKind::CoworkerChangesRequested
                        };
                        push_event_unique(
                            &mut item_events,
                            PollerEvent::bare(item.id, role_event),
                        );
                    }
                    ReviewState::Other => {}
                }
            }

            // last_change_at moves forward on a *meaningful* state change:
            //   - new head_sha (operator pushed a fix)
            //   - check_status transitioned (pending→success/failure)
            //   - merge/close transition
            // Transient poll failures and no-op repeat responses do NOT
            // bump it, so the `await_ci` timeout escalation can detect a
            // genuinely stuck-pending PR. See orchestration's timeout
            // logic for how this is consumed.
            let head_sha_changed = previous.head_sha.as_deref() != Some(pr.head.sha.as_str());
            let check_changed = previous.check_status != check_status;
            let merged_changed = previous.merged != pr.merged;
            let closed_changed = previous.closed != pr.state.eq_ignore_ascii_case("closed");
            let last_change_at = if previous.last_change_at.is_none()
                || head_sha_changed
                || check_changed
                || merged_changed
                || closed_changed
            {
                Some(Utc::now())
            } else {
                previous.last_change_at
            };

            let new_known_state = PrKnownState {
                head_sha: Some(pr.head.sha.clone()),
                check_status,
                review_state: latest_reviews,
                merged: pr.merged,
                closed: pr.state.eq_ignore_ascii_case("closed"),
                last_change_at,
            };
            if let Ok(mut states) = self.known_state.lock() {
                states.insert(key, new_known_state);
            }

            out.extend(item_events);
        }

        Ok(out)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        host_cfg: &ResolvedScmHost,
        path: &str,
    ) -> Result<Option<T>, PollError> {
        let url = format!("{}{}", host_cfg.api_base_url.trim_end_matches('/'), path);
        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", host_cfg.token))
            .header("User-Agent", "agent-hub")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| PollError::Other(anyhow!(error)))?;

        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            return Err(PollError::RateLimited);
        }
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = response
            .error_for_status()
            .map_err(|error| PollError::Other(anyhow!(error)))?;
        response
            .json::<T>()
            .await
            .map(Some)
            .map_err(|error| PollError::Other(anyhow!(error)))
    }
}

fn push_event_unique(out: &mut Vec<PollerEvent>, event: PollerEvent) {
    // De-dupe by event kind: a single poll tick should not emit the same
    // workflow event twice for the same work item even if multiple review
    // entries collapse onto the same role-bucketed event (e.g. multiple
    // approvals on the same PR).
    if !out.iter().any(|existing| existing.event == event.event) {
        out.push(event);
    }
}

/// Build the `last_ci_failure` context patch attached to a `ChecksFailed`
/// event. The shape is intentionally generic — the future
/// `triage_ci_failure` agent step reads this from `$AGENT_CONTEXT_PATH`
/// to decide whether to auto-loop or escalate, and the workflow's
/// stub-stage triage script renders failed-run names into the comment.
fn build_ci_failure_context_patch(
    host: &str,
    head_sha: &str,
    check_runs: &CheckRunsResponse,
    status: &CommitStatusResponse,
) -> JsonMap<String, JsonValue> {
    let mut failed_runs: Vec<JsonValue> = check_runs
        .check_runs
        .iter()
        .filter(|run| {
            run.status
                .as_deref()
                .is_some_and(|state| state.eq_ignore_ascii_case("completed"))
                && run
                    .conclusion
                    .as_deref()
                    .map(str::trim)
                    .map(|c| {
                        matches_ci(
                            c,
                            &[
                                "failure",
                                "cancelled",
                                "timed_out",
                                "action_required",
                                "stale",
                                "startup_failure",
                            ],
                        )
                    })
                    .unwrap_or(false)
        })
        .map(|run| {
            let mut entry = JsonMap::new();
            entry.insert(
                "kind".to_string(),
                JsonValue::String("check_run".to_string()),
            );
            if let Some(name) = run.name.as_deref() {
                entry.insert("name".to_string(), JsonValue::String(name.to_string()));
            }
            if let Some(conclusion) = run.conclusion.as_deref() {
                entry.insert(
                    "conclusion".to_string(),
                    JsonValue::String(conclusion.to_string()),
                );
            }
            if let Some(html_url) = run.html_url.as_deref() {
                entry.insert(
                    "html_url".to_string(),
                    JsonValue::String(html_url.to_string()),
                );
            }
            JsonValue::Object(entry)
        })
        .collect();

    // Commit-status-style failures (legacy CI integrations like Drone)
    // surface here as `state in {failure, error}`. Capture them with the
    // same shape so consumers don't need to special-case the source.
    failed_runs.extend(status.statuses.iter().filter_map(|entry| {
        let state = entry.state.as_deref()?;
        if !matches_ci(state, &["failure", "error"]) {
            return None;
        }
        let mut obj = JsonMap::new();
        obj.insert(
            "kind".to_string(),
            JsonValue::String("commit_status".to_string()),
        );
        if let Some(context) = entry.context.as_deref() {
            obj.insert("name".to_string(), JsonValue::String(context.to_string()));
        }
        obj.insert(
            "conclusion".to_string(),
            JsonValue::String(state.to_string()),
        );
        if let Some(target_url) = entry.target_url.as_deref() {
            obj.insert(
                "html_url".to_string(),
                JsonValue::String(target_url.to_string()),
            );
        }
        Some(JsonValue::Object(obj))
    }));

    let mut last_ci_failure = JsonMap::new();
    last_ci_failure.insert("host".to_string(), JsonValue::String(host.to_string()));
    last_ci_failure.insert(
        "head_sha".to_string(),
        JsonValue::String(head_sha.to_string()),
    );
    last_ci_failure.insert(
        "observed_at".to_string(),
        JsonValue::String(Utc::now().to_rfc3339()),
    );
    last_ci_failure.insert("failed_runs".to_string(), JsonValue::Array(failed_runs));

    let mut patch = JsonMap::new();
    patch.insert(
        "last_ci_failure".to_string(),
        JsonValue::Object(last_ci_failure),
    );
    patch
}

fn pr_key(host: &str, owner: &str, repo: &str, number: u64) -> String {
    // Host-prefixed so the same `(owner, repo, number)` on two different
    // GitHub instances doesn't collide in the known-state map.
    format!("{host}/{owner}/{repo}#{number}")
}

fn latest_reviews_by_reviewer(reviews: Vec<ReviewResponse>) -> Vec<ReviewInfo> {
    let mut by_reviewer = HashMap::<String, ReviewState>::new();
    for review in reviews {
        let Some(reviewer) = review
            .user
            .and_then(|user| user.login)
            .map(|login| login.trim().to_string())
            .filter(|login| !login.is_empty())
        else {
            continue;
        };
        let state = review
            .state
            .as_deref()
            .map(parse_review_state)
            .unwrap_or(ReviewState::Other);
        by_reviewer.insert(reviewer, state);
    }

    let mut out = by_reviewer
        .into_iter()
        .map(|(reviewer, state)| ReviewInfo { reviewer, state })
        .collect::<Vec<_>>();
    out.sort_by(|left, right| left.reviewer.cmp(&right.reviewer));
    out
}

fn parse_review_state(state: &str) -> ReviewState {
    if state.eq_ignore_ascii_case("approved") {
        return ReviewState::Approved;
    }
    if state.eq_ignore_ascii_case("changes_requested") {
        return ReviewState::ChangesRequested;
    }
    ReviewState::Other
}

fn compute_check_status(
    check_runs: &CheckRunsResponse,
    status: &CommitStatusResponse,
) -> CheckStatus {
    let mut has_pending = false;
    let mut has_failure = false;

    for check in &check_runs.check_runs {
        if !check
            .status
            .as_deref()
            .is_some_and(|state| state.eq_ignore_ascii_case("completed"))
        {
            has_pending = true;
            continue;
        }

        let conclusion = check
            .conclusion
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if matches_ci(
            conclusion,
            &[
                "failure",
                "cancelled",
                "timed_out",
                "action_required",
                "stale",
                "startup_failure",
            ],
        ) {
            has_failure = true;
            continue;
        }
        if !matches_ci(conclusion, &["success", "neutral", "skipped"]) {
            has_pending = true;
        }
    }

    // Only consider the combined status state if there are actual individual statuses.
    // GitHub returns "pending" as the combined state even when there are zero statuses,
    // which would incorrectly block repos with no CI configured.
    if !status.statuses.is_empty() {
        if let Some(state) = status.state.as_deref() {
            if matches_ci(state, &["failure", "error"]) {
                has_failure = true;
            } else if matches_ci(state, &["pending"]) {
                has_pending = true;
            }
        }
    }

    for status in &status.statuses {
        if let Some(state) = status.state.as_deref() {
            if matches_ci(state, &["failure", "error"]) {
                has_failure = true;
            } else if matches_ci(state, &["pending"]) {
                has_pending = true;
            }
        }
    }

    // Pending takes precedence over Failure: we only classify a PR's checks
    // as Failure once *all* checks have reached a terminal state and at
    // least one terminated in failure. Otherwise a single early-failing
    // check would fire `checks_failed` while five others were still
    // running, sending the workflow back to `implement` even though the
    // remaining checks might pass on their own (or be flaky and pass on
    // retry). See task #16: a single mid-flight failing run triggered a
    // spurious round-2 implement loop while the rest of CI was still
    // pending.
    if has_pending {
        CheckStatus::Pending
    } else if has_failure {
        CheckStatus::Failure
    } else {
        // Either everything reached a terminal state with no failures, or
        // there are no checks configured on this commit. Both classify as
        // Success so PRs in repos without CI integrations can advance.
        CheckStatus::Success
    }
}

fn matches_ci(value: &str, expected: &[&str]) -> bool {
    expected
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn github_pr_context(item: &WorkItem) -> Option<GitHubPrContext> {
    let github_pr = item.context.get("github_pr")?.as_object()?;
    let owner = github_pr
        .get("owner")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let repo = github_pr
        .get("repo")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let number = github_pr.get("number")?.as_u64()?;
    // Host is mandatory for multi-host polling. For backwards compat with
    // contexts written before host promotion was added, fall back to
    // `github.com` only when the URL is missing too — otherwise we'd
    // poll the wrong host. Once the orchestrator's `github_pr` promotion
    // landed, all new contexts include host directly.
    let host = github_pr
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            github_pr
                .get("html_url")
                .and_then(serde_json::Value::as_str)
                .and_then(host_from_pr_url)
        })
        .unwrap_or_else(|| "github.com".to_string());
    let owner_login = item
        .context
        .get("owner_login")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    Some(GitHubPrContext {
        host,
        owner,
        repo,
        number,
        owner_login,
    })
}

fn host_from_pr_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let rest = trimmed.split_once("://").map(|(_, rest)| rest)?;
    let host_segment = rest.split('/').next()?.trim();
    if host_segment.is_empty() {
        None
    } else {
        Some(host_segment.to_ascii_lowercase())
    }
}

struct GitHubPrContext {
    host: String,
    owner: String,
    repo: String,
    number: u64,
    owner_login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    state: String,
    #[serde(default)]
    merged: bool,
    head: PullRequestHead,
}

#[derive(Debug, Deserialize)]
struct PullRequestHead {
    sha: String,
}

#[derive(Debug, Deserialize, Default)]
struct CheckRunsResponse {
    #[serde(default)]
    check_runs: Vec<CheckRunResponse>,
}

#[derive(Debug, Deserialize)]
struct CheckRunResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CommitStatusResponse {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    statuses: Vec<CommitStatusEntry>,
}

#[derive(Debug, Deserialize)]
struct CommitStatusEntry {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewResponse {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    user: Option<ReviewUser>,
}

#[derive(Debug, Deserialize)]
struct ReviewUser {
    #[serde(default)]
    login: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: Option<&str>, conclusion: Option<&str>) -> CheckRunResponse {
        CheckRunResponse {
            status: status.map(ToOwned::to_owned),
            conclusion: conclusion.map(ToOwned::to_owned),
            name: None,
            html_url: None,
        }
    }

    fn named_run(
        status: Option<&str>,
        conclusion: Option<&str>,
        name: &str,
        html_url: &str,
    ) -> CheckRunResponse {
        CheckRunResponse {
            status: status.map(ToOwned::to_owned),
            conclusion: conclusion.map(ToOwned::to_owned),
            name: Some(name.to_string()),
            html_url: Some(html_url.to_string()),
        }
    }

    fn check_runs(runs: Vec<CheckRunResponse>) -> CheckRunsResponse {
        CheckRunsResponse { check_runs: runs }
    }

    fn empty_status() -> CommitStatusResponse {
        CommitStatusResponse::default()
    }

    fn status(state: &str, entries: &[&str]) -> CommitStatusResponse {
        CommitStatusResponse {
            state: Some(state.to_string()),
            statuses: entries
                .iter()
                .map(|s| CommitStatusEntry {
                    state: Some((*s).to_string()),
                    context: None,
                    target_url: None,
                })
                .collect(),
        }
    }

    #[test]
    fn no_signal_classifies_as_success() {
        // No checks configured at all (empty repo CI). Must not block PRs.
        assert_eq!(
            compute_check_status(&check_runs(vec![]), &empty_status()),
            CheckStatus::Success
        );
    }

    #[test]
    fn all_terminal_all_passing_classifies_as_success() {
        let runs = check_runs(vec![
            run(Some("completed"), Some("success")),
            run(Some("completed"), Some("neutral")),
            run(Some("completed"), Some("skipped")),
        ]);
        assert_eq!(
            compute_check_status(&runs, &empty_status()),
            CheckStatus::Success
        );
    }

    #[test]
    fn all_terminal_with_failure_classifies_as_failure() {
        let runs = check_runs(vec![
            run(Some("completed"), Some("success")),
            run(Some("completed"), Some("failure")),
        ]);
        assert_eq!(
            compute_check_status(&runs, &empty_status()),
            CheckStatus::Failure
        );
    }

    #[test]
    fn pending_takes_precedence_over_failure() {
        // Regression for task #16: one check has already failed but
        // another is still in_progress. We must NOT classify this as
        // Failure — that would prematurely fire `checks_failed` and loop
        // the workflow back to `implement` while the rest of CI is still
        // pending (and might pass on its own / be flake that retries
        // green).
        let runs = check_runs(vec![
            run(Some("completed"), Some("failure")),
            run(Some("in_progress"), None),
        ]);
        assert_eq!(
            compute_check_status(&runs, &empty_status()),
            CheckStatus::Pending
        );
    }

    #[test]
    fn pending_takes_precedence_when_only_queued_remain() {
        let runs = check_runs(vec![
            run(Some("completed"), Some("failure")),
            run(Some("queued"), None),
        ]);
        assert_eq!(
            compute_check_status(&runs, &empty_status()),
            CheckStatus::Pending
        );
    }

    #[test]
    fn unrecognized_conclusion_treated_as_pending() {
        // Defensive: GitHub may invent new conclusion values. Anything we
        // don't explicitly recognize as pass/fail is treated as still
        // in-flight rather than silently green.
        let runs = check_runs(vec![run(Some("completed"), Some("brand_new_state"))]);
        assert_eq!(
            compute_check_status(&runs, &empty_status()),
            CheckStatus::Pending
        );
    }

    #[test]
    fn combined_status_pending_overrides_check_runs_failure() {
        let runs = check_runs(vec![run(Some("completed"), Some("failure"))]);
        let status = status("pending", &["pending", "success"]);
        assert_eq!(compute_check_status(&runs, &status), CheckStatus::Pending);
    }

    #[test]
    fn combined_status_failure_with_all_terminal_classifies_as_failure() {
        let runs = check_runs(vec![run(Some("completed"), Some("success"))]);
        let status = status("failure", &["failure", "success"]);
        assert_eq!(compute_check_status(&runs, &status), CheckStatus::Failure);
    }

    #[test]
    fn ci_failure_patch_records_failed_runs_only() {
        let runs = check_runs(vec![
            named_run(
                Some("completed"),
                Some("success"),
                "lint",
                "https://example.test/lint",
            ),
            named_run(
                Some("completed"),
                Some("failure"),
                "go-build",
                "https://example.test/build",
            ),
            named_run(
                Some("completed"),
                Some("cancelled"),
                "integration",
                "https://example.test/integ",
            ),
            named_run(Some("in_progress"), None, "still-running", ""),
        ]);
        let status = CommitStatusResponse {
            state: Some("failure".to_string()),
            statuses: vec![
                CommitStatusEntry {
                    state: Some("failure".to_string()),
                    context: Some("drone/build".to_string()),
                    target_url: Some("https://drone.test/build/123".to_string()),
                },
                CommitStatusEntry {
                    state: Some("success".to_string()),
                    context: Some("drone/unit".to_string()),
                    target_url: None,
                },
            ],
        };

        let patch = build_ci_failure_context_patch("github.example.test", "deadbeef", &runs, &status);
        let last = patch
            .get("last_ci_failure")
            .expect("last_ci_failure key set")
            .as_object()
            .expect("object");

        assert_eq!(last.get("host").and_then(|v| v.as_str()), Some("github.example.test"));
        assert_eq!(last.get("head_sha").and_then(|v| v.as_str()), Some("deadbeef"));
        assert!(last.contains_key("observed_at"));

        let runs_arr = last
            .get("failed_runs")
            .and_then(|v| v.as_array())
            .expect("failed_runs array");
        // Only failed entries: go-build, integration, drone/build (3 total).
        assert_eq!(runs_arr.len(), 3);

        let names: Vec<&str> = runs_arr
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"go-build"));
        assert!(names.contains(&"integration"));
        assert!(names.contains(&"drone/build"));

        let kinds: Vec<&str> = runs_arr
            .iter()
            .filter_map(|v| v.get("kind").and_then(|n| n.as_str()))
            .collect();
        assert!(kinds.contains(&"check_run"));
        assert!(kinds.contains(&"commit_status"));
    }

    #[test]
    fn empty_combined_status_with_pending_state_does_not_block() {
        // GitHub returns state="pending" even when there are zero
        // statuses. We special-case this to avoid blocking repos that
        // have no commit-status integrations configured.
        let runs = check_runs(vec![run(Some("completed"), Some("success"))]);
        let status = CommitStatusResponse {
            state: Some("pending".to_string()),
            statuses: vec![],
        };
        assert_eq!(compute_check_status(&runs, &status), CheckStatus::Success);
    }
}

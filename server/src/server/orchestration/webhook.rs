use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::Utc;
use hmac::{Hmac, Mac};
use protocol::{EventSource, FireEventRequest, WorkItemState, WorkflowEventKind};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::AppError;

use super::{
    CONTEXT_GITHUB_PR_STATE_KEY, map_config, map_db, parse_pr_number_from_url,
    update_github_pr_state_in_context,
};

#[derive(Debug, Clone, Deserialize)]
pub struct GithubWebhookPayload {
    pub action: Option<String>,
    #[serde(default)]
    pub pull_request: Option<GithubPullRequestWebhook>,
    #[serde(default)]
    pub review: Option<GithubReviewWebhook>,
    #[serde(default)]
    pub sender: Option<GithubUserWebhook>,
    #[serde(default)]
    pub check_suite: Option<GithubCheckSuiteWebhook>,
    #[serde(default)]
    pub check_run: Option<GithubCheckRunWebhook>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubPullRequestWebhook {
    pub html_url: String,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub merged: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubReviewWebhook {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub user: Option<GithubUserWebhook>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubUserWebhook {
    #[serde(default)]
    pub login: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubCheckSuiteWebhook {
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<GithubPullRequestRefWebhook>,
    #[serde(default)]
    pub head_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubCheckRunWebhook {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<GithubPullRequestRefWebhook>,
    #[serde(default)]
    pub head_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubPullRequestRefWebhook {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct GithubWebhookIngestResponse {
    pub matched_jobs: usize,
    pub fired_events: usize,
    pub mapped_event: Option<WorkflowEventKind>,
}

pub async fn github_webhook<N: super::super::notifier::Notifier>(
    State(state): State<super::super::AppState<N>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, AppError> {
    if let Err(status) = authenticate_github_webhook(
        state.config.github_webhook_secret.as_deref(),
        &headers,
        &body,
    ) {
        return Ok(status.into_response());
    }

    let payload: GithubWebhookPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response()),
    };

    let result = ingest_github_event(
        &state.orchestration,
        &headers,
        &payload,
        EventSource::Webhook,
        "mapped from GitHub webhook",
    )
    .await?;

    Ok(Json(result).into_response())
}

fn authenticate_github_webhook(
    secret: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), StatusCode> {
    let Some(secret) = secret.filter(|secret| !secret.trim().is_empty()) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let signature_header = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !verify_github_signature(secret, body, signature_header) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

pub(crate) async fn ingest_github_event(
    orchestration: &super::OrchestrationService,
    headers: &HeaderMap,
    payload: &GithubWebhookPayload,
    source: EventSource,
    event_reason: &str,
) -> Result<GithubWebhookIngestResponse, AppError> {
    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let action = payload.action.as_deref().unwrap_or_default();
    let mapped_event = map_github_webhook_to_workflow_event(headers, payload, None);
    let webhook_pr = github_webhook_pr_reference(event_name, payload);
    let pr_closed_unmerged = event_name == "pull_request"
        && action == "closed"
        && !payload
            .pull_request
            .as_ref()
            .and_then(|pr| pr.merged)
            .unwrap_or(false);
    let check_name = payload
        .check_run
        .as_ref()
        .and_then(|check| check.name.as_deref());
    if webhook_pr.pr_url.is_none()
        && webhook_pr.pr_number.is_none()
        && webhook_pr.head_branch.is_none()
    {
        return Ok(GithubWebhookIngestResponse {
            matched_jobs: 0,
            fired_events: 0,
            mapped_event: None,
        });
    }
    if mapped_event.is_none() && !pr_closed_unmerged {
        return Ok(GithubWebhookIngestResponse {
            matched_jobs: 0,
            fired_events: 0,
            mapped_event: None,
        });
    }

    let items = orchestration.list_work_items().await?;
    let matched = items
        .into_iter()
        .filter(|item| {
            let webhook_matches_item = item_matches_github_webhook(item, &webhook_pr);
            !matches!(
                item.state,
                WorkItemState::Succeeded | WorkItemState::Failed | WorkItemState::Cancelled
            ) && webhook_matches_item
        })
        .collect::<Vec<_>>();

    let mut fired = 0usize;
    for item in &matched {
        let mut updated_item = item.clone();
        let pr_url = webhook_pr
            .pr_url
            .clone()
            .or_else(|| item_matching_pr_url(item, webhook_pr.pr_number))
            .unwrap_or_default();
        let owner_github_login = owner_github_login_from_context(&item.context);
        let mapped_event_for_item =
            map_github_webhook_to_workflow_event(headers, payload, owner_github_login.as_deref());
        let pr_number = webhook_pr
            .pr_number
            .or_else(|| parse_pr_number_from_url(&pr_url));
        update_github_pr_state_in_context(
            &mut updated_item.context,
            &pr_url,
            pr_number,
            webhook_pr.head_branch.as_deref(),
            mapped_event_for_item.as_ref(),
            check_name,
            pr_closed_unmerged,
            Utc::now(),
        );
        updated_item.updated_at = Utc::now();
        orchestration
            .update_work_item(&updated_item)
            .await
            .map_err(map_db)?;
        orchestration
            .persist_task_projection(&updated_item, None)
            .await
            .map_err(map_config)?;

        let mut candidates = Vec::new();
        if let Some(event) = mapped_event_for_item.as_ref() {
            candidates.extend(map_role_event_to_fallback_candidates(event));
        }

        for candidate in candidates {
            if orchestration
                .fire_event_trusted_internal_with_source(
                    item.id,
                    FireEventRequest {
                        event: candidate,
                        job_id: None,
                        reason: Some(event_reason.to_string()),
                    },
                    source.clone(),
                )
                .await
                .is_ok()
            {
                fired = fired.saturating_add(1);
                break;
            }
        }
    }

    Ok(GithubWebhookIngestResponse {
        matched_jobs: matched.len(),
        fired_events: fired,
        mapped_event,
    })
}

fn verify_github_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(signature_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };

    let Ok(signature) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);

    mac.verify_slice(&signature).is_ok()
}

pub(crate) fn owner_github_login_from_context(context: &serde_json::Value) -> Option<String> {
    context
        .get("owner_github_login")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(ToOwned::to_owned)
}

fn github_reviewer_login_from_payload(payload: &GithubWebhookPayload) -> Option<&str> {
    payload
        .review
        .as_ref()
        .and_then(|review| review.user.as_ref())
        .and_then(|user| user.login.as_deref())
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .or_else(|| {
            payload
                .sender
                .as_ref()
                .and_then(|sender| sender.login.as_deref())
                .map(str::trim)
                .filter(|login| !login.is_empty())
        })
}

pub(crate) fn map_role_event_to_fallback_candidates(
    event: &WorkflowEventKind,
) -> Vec<WorkflowEventKind> {
    match event {
        WorkflowEventKind::OwnerPrApproved | WorkflowEventKind::CoworkerPrApproved => {
            vec![
                event.clone(),
                WorkflowEventKind::PrApproved,
                WorkflowEventKind::HumanApproved,
            ]
        }
        WorkflowEventKind::OwnerChangesRequested | WorkflowEventKind::CoworkerChangesRequested => {
            vec![
                event.clone(),
                WorkflowEventKind::PrChangesRequested,
                WorkflowEventKind::HumanChangesRequested,
            ]
        }
        WorkflowEventKind::PrApproved => {
            vec![event.clone(), WorkflowEventKind::HumanApproved]
        }
        WorkflowEventKind::PrChangesRequested => {
            vec![event.clone(), WorkflowEventKind::HumanChangesRequested]
        }
        WorkflowEventKind::PrMerged => {
            vec![event.clone(), WorkflowEventKind::JobSucceeded]
        }
        WorkflowEventKind::ChecksPassed => {
            vec![event.clone(), WorkflowEventKind::JobSucceeded]
        }
        WorkflowEventKind::ChecksFailed => {
            vec![event.clone(), WorkflowEventKind::JobFailed]
        }
        _ => vec![event.clone()],
    }
}

pub(crate) fn map_github_webhook_to_workflow_event(
    headers: &HeaderMap,
    payload: &GithubWebhookPayload,
    owner_github_login: Option<&str>,
) -> Option<WorkflowEventKind> {
    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let action = payload.action.as_deref().unwrap_or_default();

    if event_name == "pull_request" {
        if matches!(action, "opened" | "reopened") {
            return Some(WorkflowEventKind::PrCreated);
        }
        if action == "closed"
            && payload
                .pull_request
                .as_ref()
                .and_then(|pr| pr.merged)
                .unwrap_or(false)
        {
            return Some(WorkflowEventKind::PrMerged);
        }
    }

    if event_name == "pull_request_review" && action == "submitted" {
        let reviewer = github_reviewer_login_from_payload(payload);
        let reviewer_is_owner = reviewer
            .zip(owner_github_login)
            .is_some_and(|(reviewer, owner)| reviewer.eq_ignore_ascii_case(owner));

        match payload.review.as_ref().and_then(|r| r.state.as_deref()) {
            Some("approved") => {
                if owner_github_login.is_some() {
                    return Some(if reviewer_is_owner {
                        WorkflowEventKind::OwnerPrApproved
                    } else {
                        WorkflowEventKind::CoworkerPrApproved
                    });
                }
                return Some(WorkflowEventKind::PrApproved);
            }
            Some("changes_requested") => {
                if owner_github_login.is_some() {
                    return Some(if reviewer_is_owner {
                        WorkflowEventKind::OwnerChangesRequested
                    } else {
                        WorkflowEventKind::CoworkerChangesRequested
                    });
                }
                return Some(WorkflowEventKind::PrChangesRequested);
            }
            _ => {}
        }
    }

    if event_name == "check_suite" && action == "completed" {
        match payload
            .check_suite
            .as_ref()
            .and_then(|c| c.conclusion.as_deref())
        {
            Some("success") => return Some(WorkflowEventKind::ChecksPassed),
            Some("failure" | "timed_out" | "cancelled") => {
                return Some(WorkflowEventKind::ChecksFailed);
            }
            _ => {}
        }
    }

    if event_name == "check_run" && action == "completed" {
        match payload
            .check_run
            .as_ref()
            .and_then(|c| c.conclusion.as_deref())
        {
            Some("success") => return Some(WorkflowEventKind::ChecksPassed),
            Some("failure" | "timed_out" | "cancelled") => {
                return Some(WorkflowEventKind::ChecksFailed);
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GithubWebhookPrReference {
    pr_url: Option<String>,
    pr_number: Option<u64>,
    head_branch: Option<String>,
}

pub(crate) fn github_webhook_pr_reference(
    event_name: &str,
    payload: &GithubWebhookPayload,
) -> GithubWebhookPrReference {
    let top_level_pr_url = payload.pull_request.as_ref().map(|pr| pr.html_url.clone());
    let top_level_pr_number = payload.pull_request.as_ref().and_then(|pr| pr.number);

    let check_suite_pr_url = payload
        .check_suite
        .as_ref()
        .and_then(|suite| suite.pull_requests.first())
        .map(|pr| pr.url.clone());
    let check_run_pr_url = payload
        .check_run
        .as_ref()
        .and_then(|check| check.pull_requests.first())
        .map(|pr| pr.url.clone());

    let pr_url = top_level_pr_url
        .or_else(|| check_suite_pr_url.clone())
        .or_else(|| check_run_pr_url.clone());
    let pr_number = top_level_pr_number
        .or_else(|| pr_url.as_deref().and_then(parse_pr_number_from_url))
        .or_else(|| {
            check_suite_pr_url
                .as_deref()
                .and_then(parse_pr_number_from_url)
        })
        .or_else(|| {
            check_run_pr_url
                .as_deref()
                .and_then(parse_pr_number_from_url)
        });
    let head_branch = if event_name == "check_suite" {
        payload
            .check_suite
            .as_ref()
            .and_then(|suite| suite.head_branch.clone())
    } else if event_name == "check_run" {
        payload
            .check_run
            .as_ref()
            .and_then(|check| check.head_branch.clone())
    } else {
        None
    };

    GithubWebhookPrReference {
        pr_url,
        pr_number,
        head_branch,
    }
}

fn item_github_pr_urls(item: &protocol::WorkItem) -> impl Iterator<Item = &str> {
    let pr_state_url = item
        .context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .and_then(serde_json::Value::as_object)
        .and_then(|state| state.get("url"))
        .and_then(serde_json::Value::as_str);

    std::iter::once(pr_state_url).flatten().chain(
        item.context
            .get("job_results")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|results| results.values())
            .filter_map(|result| {
                result
                    .get("github_pr")
                    .and_then(|pr| pr.get("url"))
                    .and_then(serde_json::Value::as_str)
            }),
    )
}

fn item_github_head_branches(item: &protocol::WorkItem) -> impl Iterator<Item = &str> {
    let pr_state_head_branch = item
        .context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .and_then(serde_json::Value::as_object)
        .and_then(|state| state.get("head_branch"))
        .and_then(serde_json::Value::as_str);

    std::iter::once(pr_state_head_branch).flatten().chain(
        item.context
            .get("job_results")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|results| results.values())
            .flat_map(|result| {
                let pr_head_branch = result
                    .get("github_pr")
                    .and_then(|pr| pr.get("head_ref_name"))
                    .and_then(serde_json::Value::as_str);
                let repo_target_branch = result
                    .get("repo")
                    .and_then(|repo| repo.get("target_branch"))
                    .and_then(serde_json::Value::as_str);
                let repo_branch = result
                    .get("repo")
                    .and_then(|repo| repo.get("branch"))
                    .and_then(serde_json::Value::as_str);

                [pr_head_branch, repo_target_branch, repo_branch]
                    .into_iter()
                    .flatten()
            }),
    )
}

pub(crate) fn item_matching_pr_url(
    item: &protocol::WorkItem,
    pr_number: Option<u64>,
) -> Option<String> {
    item_github_pr_urls(item)
        .find(|url| {
            pr_number
                .map(|number| parse_pr_number_from_url(url) == Some(number))
                .unwrap_or(true)
        })
        .map(ToOwned::to_owned)
}

pub(crate) fn item_matches_github_webhook(
    item: &protocol::WorkItem,
    webhook_pr: &GithubWebhookPrReference,
) -> bool {
    let pr_url_match = webhook_pr.pr_url.as_deref().is_some_and(|incoming_url| {
        item_github_pr_urls(item).any(|url| {
            url == incoming_url
                || webhook_pr
                    .pr_number
                    .is_some_and(|number| parse_pr_number_from_url(url) == Some(number))
        })
    });

    let pr_number_match = webhook_pr.pr_number.is_some_and(|number| {
        item_github_pr_urls(item).any(|url| parse_pr_number_from_url(url) == Some(number))
    });

    let branch_match = webhook_pr.head_branch.as_deref().is_some_and(|branch| {
        let repo_match = item
            .repo
            .as_ref()
            .map(|repo| {
                repo.branch_name.as_deref() == Some(branch) || repo.base_ref.as_str() == branch
            })
            .unwrap_or(false);
        repo_match || item_github_head_branches(item).any(|head_branch| head_branch == branch)
    });

    pr_url_match || pr_number_match || branch_match
}

#[cfg(test)]
pub(super) fn map_github_webhook_to_workflow_event_for_tests(
    headers: &HeaderMap,
    payload: &GithubWebhookPayload,
    owner_github_login: Option<&str>,
) -> Option<WorkflowEventKind> {
    map_github_webhook_to_workflow_event(headers, payload, owner_github_login)
}

#[cfg(test)]
pub(super) fn github_webhook_pr_reference_for_tests(
    event_name: &str,
    payload: &GithubWebhookPayload,
) -> (Option<String>, Option<u64>, Option<String>) {
    let reference = github_webhook_pr_reference(event_name, payload);
    (reference.pr_url, reference.pr_number, reference.head_branch)
}

#[cfg(test)]
pub(super) fn item_matches_github_webhook_for_tests(
    item: &protocol::WorkItem,
    pr_url: Option<String>,
    pr_number: Option<u64>,
    head_branch: Option<String>,
) -> bool {
    item_matches_github_webhook(
        item,
        &GithubWebhookPrReference {
            pr_url,
            pr_number,
            head_branch,
        },
    )
}

#[cfg(test)]
pub(super) fn verify_github_signature_for_tests(
    secret: &str,
    body: &[u8],
    signature_header: &str,
) -> bool {
    verify_github_signature(secret, body, signature_header)
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn missing_blank_and_unsigned_webhooks_are_rejected() {
        let headers = HeaderMap::new();
        assert_eq!(
            authenticate_github_webhook(None, &headers, b"{}"),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            authenticate_github_webhook(Some("  "), &headers, b"{}"),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            authenticate_github_webhook(Some("secret"), &headers, b"{}"),
            Err(StatusCode::UNAUTHORIZED)
        );
    }
}

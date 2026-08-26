use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use protocol::{
    QuestionDecision, QuestionGatewayOutput, QuestionProxyRequest, QuestionProxyResponse,
    QuestionResolveRequest, QuestionStatus, QuestionWaitResponse, Secret,
};

/// Arguments for the `question` subcommand.
#[derive(clap::Args)]
pub struct QuestionArgs {
    /// Use Opencode provider (only provider supported for questions)
    #[arg(long)]
    pub opencode: bool,

    /// Server URL (e.g. https://hub.example.com)
    #[arg(long, env = "AGENT_HUB_SERVER")]
    pub server: String,

    /// Bearer token for server auth
    #[arg(long, env = "AGENT_HUB_TOKEN")]
    pub token: Secret,

    /// Maximum time to wait for an answer in seconds
    #[arg(long, default_value = "86400")]
    pub timeout: u64,
}

/// Entry point for `agent-hub-gateway question`.
///
/// Reads a `QuestionProxyRequest` JSON from stdin, registers it with the
/// server, long-polls until answered/rejected, then writes the result to
/// stdout and exits.
///
/// Exit codes:
///   0 = answered — stdout contains `{"answers": [[...], ...]}` JSON
///   1 = explicitly rejected
///   2 = fail-closed (bad input)
///   3 = timed out after a healthy pending wait
///   4 = approvals-pipeline error
pub async fn run(args: QuestionArgs) -> ExitCode {
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("agent-hub-gateway question: failed to read stdin: {e}");
        return ExitCode::from(2);
    }

    let req: QuestionProxyRequest = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agent-hub-gateway question: invalid request JSON: {e}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "[info] question: session={} questions={} request_id={}",
        req.session_id,
        req.questions.len(),
        req.question_request_id
    );

    match proxy_question(&args, &req).await {
        Ok(answers) => {
            let out = serde_json::to_string(&QuestionGatewayOutput { answers })
                .expect("QuestionGatewayOutput serialization failed");
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(QuestionFlowError::Rejected(reason)) => {
            eprintln!("agent-hub-gateway question: {reason}");
            ExitCode::from(1)
        }
        Err(QuestionFlowError::Timeout) => {
            eprintln!("agent-hub-gateway question: question timed out");
            ExitCode::from(3)
        }
        Err(QuestionFlowError::Pipeline(reason)) => {
            eprintln!("agent-hub-gateway question: {reason}");
            ExitCode::from(4)
        }
    }
}

#[derive(Debug, PartialEq)]
enum QuestionFlowError {
    Rejected(String),
    Timeout,
    Pipeline(String),
}

/// POST the question to the server then long-poll until answered.
async fn proxy_question(
    args: &QuestionArgs,
    req: &QuestionProxyRequest,
) -> Result<Vec<Vec<String>>, QuestionFlowError> {
    let client = reqwest::Client::new();
    let base = args.server.trim_end_matches('/');

    let register_url = format!("{base}/api/v1/hooks/question");
    eprintln!(
        "[info] registering question: session={} questions={}",
        req.session_id,
        req.questions.len()
    );

    let register_resp = client
        .post(&register_url)
        .bearer_auth(args.token.expose())
        .json(req)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| QuestionFlowError::Pipeline(format!("failed to register question: {e}")))?;

    if !register_resp.status().is_success() {
        return Err(QuestionFlowError::Pipeline(format!(
            "server returned {} for question registration",
            register_resp.status()
        )));
    }

    let proxy_resp: QuestionProxyResponse = register_resp.json().await.map_err(|e| {
        QuestionFlowError::Pipeline(format!("failed to parse question response: {e}"))
    })?;

    // Check if already resolved (e.g. idempotency replay)
    if !matches!(proxy_resp.status, QuestionStatus::Pending) {
        return resolved_question_status(proxy_resp.status);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let wait_url = format!("{base}/api/v1/questions/{}/wait", proxy_resp.id);
    let cancel_url = format!("{base}/api/v1/questions/{}/resolve", proxy_resp.id);

    eprintln!(
        "[info] waiting for question answer (id={}, timeout={}s)",
        proxy_resp.id, args.timeout
    );

    tokio::select! {
        result = poll_for_answer(&client, &wait_url, &args.token, deadline, args.timeout) => result,
        _ = shutdown_signal() => {
            eprintln!("[info] received shutdown signal, cancelling question {}", proxy_resp.id);
            let _ = client
                .post(&cancel_url)
                .bearer_auth(args.token.expose())
                .json(&QuestionResolveRequest {
                    decision: QuestionDecision::Cancel,
                    answers: None,
                    reason: Some("aborted by signal".to_string()),
                })
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            Err(QuestionFlowError::Pipeline("aborted by signal".to_string()))
        }
    }
}

/// Long-poll the server until the question is resolved or global timeout expires.
async fn poll_for_answer(
    client: &reqwest::Client,
    wait_url: &str,
    token: &Secret,
    deadline: tokio::time::Instant,
    timeout_secs: u64,
) -> Result<Vec<Vec<String>>, QuestionFlowError> {
    let mut attempt: u32 = 0;
    let mut last_wait_was_healthy = true;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("[error] question timed out after {timeout_secs}s with no answer");
            return Err(question_deadline_error(last_wait_was_healthy));
        }

        attempt += 1;

        let resp = await_question_deadline(
            deadline,
            last_wait_was_healthy,
            client
                .get(wait_url)
                .bearer_auth(token.expose())
                .timeout(Duration::from_secs(60))
                .send(),
        )
        .await?;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[warn] question wait failed (attempt {attempt}, retrying in 2s): {e}");
                last_wait_was_healthy = false;
                await_question_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        let http_status = resp.status();

        if !http_status.is_success() {
            if http_status.is_client_error() {
                return Err(QuestionFlowError::Pipeline(format!(
                    "server returned {http_status} for question wait"
                )));
            }
            eprintln!(
                "[warn] server returned {http_status} for question wait (attempt {attempt}, retrying in 2s)"
            );
            last_wait_was_healthy = false;
            await_question_deadline(
                deadline,
                last_wait_was_healthy,
                tokio::time::sleep(Duration::from_secs(2)),
            )
            .await?;
            continue;
        }

        let body = match await_question_deadline(deadline, last_wait_was_healthy, resp.text())
            .await?
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[warn] failed to read question wait body (attempt {attempt}, retrying in 2s): {e}"
                );
                last_wait_was_healthy = false;
                await_question_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        let wait: QuestionWaitResponse = match serde_json::from_str(&body) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
                    "[warn] failed to parse question wait response (attempt {attempt}, retrying in 2s): {e}"
                );
                last_wait_was_healthy = false;
                await_question_deadline(
                    deadline,
                    last_wait_was_healthy,
                    tokio::time::sleep(Duration::from_secs(2)),
                )
                .await?;
                continue;
            }
        };

        ensure_question_before_deadline(deadline, last_wait_was_healthy)?;

        if http_status.as_u16() == 202 {
            if !matches!(wait.status, QuestionStatus::Pending) {
                return Err(QuestionFlowError::Pipeline(format!(
                    "server returned {http_status} with non-pending question status"
                )));
            }
            last_wait_was_healthy = true;
            let secs = remaining.as_secs();
            eprintln!("[info] question still pending (attempt {attempt}, {secs}s remaining)");
            continue;
        }

        return resolved_question_status(wait.status);
    }
}

fn resolved_question_status(status: QuestionStatus) -> Result<Vec<Vec<String>>, QuestionFlowError> {
    match status {
        QuestionStatus::Answered { answers } => Ok(answers),
        QuestionStatus::Rejected { reason } => Err(QuestionFlowError::Rejected(
            reason.unwrap_or_else(|| "rejected by operator".to_string()),
        )),
        QuestionStatus::Cancelled => Err(QuestionFlowError::Pipeline(
            "question cancelled".to_string(),
        )),
        QuestionStatus::Pending => Err(QuestionFlowError::Pipeline(
            "server returned an unexpected final pending question status".to_string(),
        )),
    }
}

fn question_deadline_error(last_wait_was_healthy: bool) -> QuestionFlowError {
    if last_wait_was_healthy {
        QuestionFlowError::Timeout
    } else {
        QuestionFlowError::Pipeline("question wait failed until the deadline".to_string())
    }
}

fn ensure_question_before_deadline(
    deadline: tokio::time::Instant,
    last_wait_was_healthy: bool,
) -> Result<(), QuestionFlowError> {
    if tokio::time::Instant::now() >= deadline {
        Err(question_deadline_error(last_wait_was_healthy))
    } else {
        Ok(())
    }
}

async fn await_question_deadline<F>(
    deadline: tokio::time::Instant,
    last_wait_was_healthy: bool,
    future: F,
) -> Result<F::Output, QuestionFlowError>
where
    F: std::future::Future,
{
    ensure_question_before_deadline(deadline, last_wait_was_healthy)?;
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| question_deadline_error(last_wait_was_healthy))
}

/// Wait for SIGTERM or SIGINT.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c handler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_rejection_is_distinct_from_indeterminate_outcomes() {
        assert!(matches!(
            resolved_question_status(QuestionStatus::Rejected {
                reason: Some("no".to_string()),
            }),
            Err(QuestionFlowError::Rejected(_))
        ));
        assert!(matches!(
            resolved_question_status(QuestionStatus::Cancelled),
            Err(QuestionFlowError::Pipeline(_))
        ));
        assert!(matches!(
            resolved_question_status(QuestionStatus::Pending),
            Err(QuestionFlowError::Pipeline(_))
        ));
        assert_eq!(question_deadline_error(true), QuestionFlowError::Timeout);
        assert!(matches!(
            question_deadline_error(false),
            QuestionFlowError::Pipeline(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn question_deadline_rejects_late_answer_after_healthy_pending() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = await_question_deadline(deadline, true, async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            vec![vec!["late answer".to_string()]]
        })
        .await;

        assert_eq!(result.unwrap_err(), QuestionFlowError::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn question_deadline_after_recurring_failure_is_pipeline_error() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = await_question_deadline(deadline, false, async {
            tokio::time::sleep(Duration::from_secs(6)).await;
        })
        .await;

        assert!(matches!(result, Err(QuestionFlowError::Pipeline(_))));
    }
}

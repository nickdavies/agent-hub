use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use crate::RunError;
use protocol::{
    QuestionDecision, QuestionProxyRequest, QuestionProxyResponse, QuestionResolveRequest,
    QuestionStatus, QuestionWaitResponse, Secret,
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
    #[arg(long, default_value = "600")]
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
///   1 = rejected, cancelled, timed out, or server unreachable
///   2 = fail-closed (bad input)
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
            let out = serde_json::json!({ "answers": answers });
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(RunError::ServerUnreachable(e)) => {
            eprintln!("agent-hub-gateway question: {e}");
            ExitCode::from(1)
        }
        Err(RunError::FailClosed(e)) => {
            eprintln!("agent-hub-gateway question: {e}");
            ExitCode::from(2)
        }
    }
}

/// POST the question to the server then long-poll until answered.
async fn proxy_question(
    args: &QuestionArgs,
    req: &QuestionProxyRequest,
) -> Result<Vec<Vec<String>>, RunError> {
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
        .map_err(|e| RunError::ServerUnreachable(format!("failed to register question: {e}")))?;

    if !register_resp.status().is_success() {
        return Err(RunError::ServerUnreachable(format!(
            "server returned {} for question registration",
            register_resp.status()
        )));
    }

    let proxy_resp: QuestionProxyResponse = register_resp.json().await.map_err(|e| {
        RunError::ServerUnreachable(format!("failed to parse question response: {e}"))
    })?;

    // Check if already resolved (e.g. idempotency replay)
    match proxy_resp.status {
        QuestionStatus::Answered { answers } => return Ok(answers),
        QuestionStatus::Rejected { .. } | QuestionStatus::Cancelled => {
            return Err(RunError::ServerUnreachable("question rejected".to_string()));
        }
        QuestionStatus::Pending => {}
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
            Err(RunError::ServerUnreachable("aborted by signal".to_string()))
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
) -> Result<Vec<Vec<String>>, RunError> {
    let mut attempt: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("[error] question timed out after {timeout_secs}s with no answer");
            return Err(RunError::ServerUnreachable(
                "question timed out".to_string(),
            ));
        }

        attempt += 1;

        let resp = client
            .get(wait_url)
            .bearer_auth(token.expose())
            .timeout(Duration::from_secs(60))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[warn] question wait failed (attempt {attempt}, retrying in 2s): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let http_status = resp.status();

        if !http_status.is_success() {
            if http_status.is_client_error() {
                return Err(RunError::ServerUnreachable(format!(
                    "server returned {http_status} for question wait"
                )));
            }
            eprintln!(
                "[warn] server returned {http_status} for question wait (attempt {attempt}, retrying in 2s)"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[warn] failed to read question wait body (attempt {attempt}, retrying in 2s): {e}"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let wait: QuestionWaitResponse = match serde_json::from_str(&body) {
            Ok(w) => w,
            Err(e) => {
                eprintln!(
                    "[warn] failed to parse question wait response (attempt {attempt}, retrying in 2s): {e}"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if http_status.as_u16() == 202 || matches!(wait.status, QuestionStatus::Pending) {
            let secs = remaining.as_secs();
            eprintln!("[info] question still pending (attempt {attempt}, {secs}s remaining)");
            continue;
        }

        return match wait.status {
            QuestionStatus::Answered { answers } => {
                eprintln!("[info] question answered");
                Ok(answers)
            }
            QuestionStatus::Rejected { reason } => {
                eprintln!("[info] question rejected: {:?}", reason);
                Err(RunError::ServerUnreachable(
                    reason.unwrap_or_else(|| "rejected by operator".to_string()),
                ))
            }
            QuestionStatus::Cancelled => {
                eprintln!("[info] question cancelled");
                Err(RunError::ServerUnreachable("cancelled".to_string()))
            }
            QuestionStatus::Pending => {
                // Should not reach here — 202 case handled above.
                eprintln!("[warn] question still pending despite 200 response, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
    }
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

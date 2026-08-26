use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{
    ApprovalRequest, ApprovalResponse, ApprovalStatus, ApprovalWaitResponse, QuestionProxyRequest,
    QuestionProxyResponse, QuestionStatus, QuestionWaitResponse, RequestType, Tool,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

const APPROVAL_ID: Uuid = Uuid::from_u128(1);
const QUESTION_ID: Uuid = Uuid::from_u128(2);
// Black-box expectations duplicate the wire messages so production changes cannot silently update them.
const TIMEOUT_REASON: &str = "Your approval request timed out because the operator was away. This is not a denial. Feel free to retry if you still need this.";
const PIPELINE_REASON: &str = "There was an error in the approvals pipeline. This is a null answer: it is neither an approval nor a rejection of your intended action. Do not take this as a signal that your intended path is unwanted. Try again; if the error recurs, abort and report it to the operator.";

#[derive(Clone, Copy, Debug)]
enum Scenario {
    ApprovalDenied,
    ApprovalTimeout,
    ApprovalPipelineError,
    QuestionRejected,
    QuestionTimeout,
    QuestionPipelineError,
}

#[derive(Clone)]
struct AppState {
    scenario: Scenario,
    registrations: Arc<AtomicUsize>,
    waits: Arc<AtomicUsize>,
    unexpected_requests: Arc<AtomicUsize>,
    release_held_waits: Arc<Notify>,
}

impl AppState {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            registrations: Arc::new(AtomicUsize::new(0)),
            waits: Arc::new(AtomicUsize::new(0)),
            unexpected_requests: Arc::new(AtomicUsize::new(0)),
            release_held_waits: Arc::new(Notify::new()),
        }
    }
}

struct TestServer {
    url: String,
    state: AppState,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(scenario: Scenario) -> Self {
        let state = AppState::new(scenario);
        let app = Router::new()
            .route("/api/v1/hooks/approval", post(register_approval))
            .route("/api/v1/approvals/{id}/wait", get(wait_for_approval))
            .route("/api/v1/hooks/question", post(register_question))
            .route("/api/v1/questions/{id}/wait", get(wait_for_question))
            .fallback(unexpected_request)
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener binds");
        let address = listener.local_addr().expect("listener has a local address");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        Self {
            url: format!("http://{address}"),
            state,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        self.state.release_held_waits.notify_one();
        let _ = self.shutdown.send(());
        let mut task = self.task;
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => result
                .expect("server task joins")
                .expect("server exits cleanly"),
            Err(_) => {
                task.abort();
                let _ = task.await;
                panic!("scripted server did not shut down within two seconds");
            }
        }
    }
}

struct GatewayOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

async fn run_gateway(server: &TestServer, command: &str, stdin: &str) -> GatewayOutput {
    let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway crate has a repository parent");
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"));
    process
        .arg(command)
        .arg("--opencode")
        .arg("--server")
        .arg(&server.url)
        .arg("--token")
        .arg("test-token");
    if command == "approval" {
        process
            .arg("--config")
            .arg("gateway/config/test-local.json");
    }
    let mut child = process
        .arg("--timeout")
        .arg("2")
        .current_dir(repository_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("gateway binary starts");
    let mut stdout = child.stdout.take().expect("gateway stdout is piped");
    let mut stderr = child.stderr.take().expect("gateway stderr is piped");
    let stdout_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout
            .read_to_end(&mut output)
            .await
            .expect("gateway stdout is readable");
        output
    });
    let stderr_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stderr
            .read_to_end(&mut output)
            .await
            .expect("gateway stderr is readable");
        output
    });
    child
        .stdin
        .take()
        .expect("gateway stdin is piped")
        .write_all(stdin.as_bytes())
        .await
        .expect("gateway stdin accepts JSON");

    let (status, timed_out) = match tokio::time::timeout(Duration::from_secs(8), child.wait()).await
    {
        Ok(status) => (status.expect("gateway status is captured"), false),
        Err(_) => {
            let _ = child.start_kill();
            (
                child.wait().await.expect("timed-out gateway is reaped"),
                true,
            )
        }
    };
    let stdout = stdout_task.await.expect("stdout reader joins");
    let stderr = stderr_task.await.expect("stderr reader joins");
    GatewayOutput {
        status,
        stdout: String::from_utf8(stdout).expect("gateway stdout is UTF-8"),
        stderr: String::from_utf8(stderr).expect("gateway stderr is UTF-8"),
        timed_out,
    }
}

fn approval_input() -> String {
    serde_json::json!({
        "session_id": "integration-session",
        "session_title": "HTTP outcomes",
        "tool_name": "mcp_test_action",
        "tool_input": {"argument": "value"},
        "cwd": "/workspace",
        "workspace_roots": ["/workspace"],
        "hook_event_name": "tool.execute.before"
    })
    .to_string()
}

fn question_input() -> String {
    serde_json::json!({
        "id": "gateway-question-id",
        "session_id": "integration-session",
        "session_display_name": "HTTP outcomes",
        "cwd": "/workspace",
        "question_request_id": "opencode-question-id",
        "questions": [{
            "question": "Continue?",
            "header": "Decision",
            "options": [{"label": "Yes", "description": "Continue"}],
            "multiple": false,
            "custom": true
        }],
        "provider": "opencode"
    })
    .to_string()
}

fn assert_authorization(headers: &HeaderMap) {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-token")
    );
}

async fn register_approval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ApprovalRequest>,
) -> Json<ApprovalResponse> {
    assert_authorization(&headers);
    assert!(matches!(
        state.scenario,
        Scenario::ApprovalDenied | Scenario::ApprovalTimeout | Scenario::ApprovalPipelineError
    ));
    state.registrations.fetch_add(1, Ordering::SeqCst);
    assert!(Uuid::parse_str(&request.id).is_ok());
    assert_eq!(request.session_id, "integration-session");
    assert_eq!(request.session_display_name, "HTTP outcomes");
    assert_eq!(request.cwd, "/workspace");
    assert_eq!(request.tool, Tool::Unknown("mcp_test_action".to_string()));
    assert_eq!(request.tool_input, serde_json::json!({"argument": "value"}));
    assert_eq!(request.provider, "opencode");
    assert_eq!(request.request_type, RequestType::ToolUse);
    assert_eq!(request.context.workspace_roots, ["/workspace"]);
    assert_eq!(
        request.context.hook_event_name.as_str(),
        "tool.execute.before"
    );
    assert!(request.context.extra.is_none());

    Json(ApprovalResponse {
        id: APPROVAL_ID,
        status: ApprovalStatus::Pending,
    })
}

async fn wait_for_approval(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> (StatusCode, Json<ApprovalWaitResponse>) {
    assert_authorization(&headers);
    assert_eq!(id, APPROVAL_ID);
    let attempt = state.waits.fetch_add(1, Ordering::SeqCst);
    let pending = || {
        (
            StatusCode::ACCEPTED,
            Json(ApprovalWaitResponse {
                status: ApprovalStatus::Pending,
            }),
        )
    };

    match state.scenario {
        Scenario::ApprovalDenied => (
            StatusCode::OK,
            Json(ApprovalWaitResponse {
                status: ApprovalStatus::Denied {
                    reason: "operator denied this".to_string(),
                },
            }),
        ),
        Scenario::ApprovalTimeout if attempt == 0 => pending(),
        Scenario::ApprovalTimeout => {
            state.release_held_waits.notified().await;
            pending()
        }
        Scenario::ApprovalPipelineError if attempt == 0 => pending(),
        Scenario::ApprovalPipelineError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApprovalWaitResponse {
                status: ApprovalStatus::Pending,
            }),
        ),
        _ => panic!("approval wait called for {:?}", state.scenario),
    }
}

async fn register_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QuestionProxyRequest>,
) -> Json<QuestionProxyResponse> {
    assert_authorization(&headers);
    assert!(matches!(
        state.scenario,
        Scenario::QuestionRejected | Scenario::QuestionTimeout | Scenario::QuestionPipelineError
    ));
    state.registrations.fetch_add(1, Ordering::SeqCst);
    assert_eq!(request.id, "gateway-question-id");
    assert_eq!(request.session_id, "integration-session");
    assert_eq!(request.session_display_name, "HTTP outcomes");
    assert_eq!(request.cwd, "/workspace");
    assert_eq!(request.question_request_id, "opencode-question-id");
    assert_eq!(request.provider, "opencode");
    assert_eq!(request.questions.len(), 1);
    assert_eq!(request.questions[0].question, "Continue?");
    assert_eq!(request.questions[0].header, "Decision");
    assert_eq!(request.questions[0].options.len(), 1);
    assert_eq!(request.questions[0].options[0].label, "Yes");
    assert_eq!(request.questions[0].options[0].description, "Continue");
    assert_eq!(request.questions[0].multiple, Some(false));
    assert_eq!(request.questions[0].custom, Some(true));

    Json(QuestionProxyResponse {
        id: QUESTION_ID,
        status: QuestionStatus::Pending,
    })
}

async fn wait_for_question(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> (StatusCode, Json<QuestionWaitResponse>) {
    assert_authorization(&headers);
    assert_eq!(id, QUESTION_ID);
    let attempt = state.waits.fetch_add(1, Ordering::SeqCst);
    let pending = || {
        (
            StatusCode::ACCEPTED,
            Json(QuestionWaitResponse {
                status: QuestionStatus::Pending,
            }),
        )
    };

    match state.scenario {
        Scenario::QuestionRejected => (
            StatusCode::OK,
            Json(QuestionWaitResponse {
                status: QuestionStatus::Rejected {
                    reason: Some("operator rejected this".to_string()),
                },
            }),
        ),
        Scenario::QuestionTimeout if attempt == 0 => pending(),
        Scenario::QuestionTimeout => {
            state.release_held_waits.notified().await;
            pending()
        }
        Scenario::QuestionPipelineError if attempt == 0 => pending(),
        Scenario::QuestionPipelineError => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(QuestionWaitResponse {
                status: QuestionStatus::Pending,
            }),
        ),
        _ => panic!("question wait called for {:?}", state.scenario),
    }
}

async fn unexpected_request(State(state): State<AppState>, method: Method, uri: Uri) -> StatusCode {
    state.unexpected_requests.fetch_add(1, Ordering::SeqCst);
    eprintln!("unexpected request: {method} {uri}");
    StatusCode::NOT_FOUND
}

fn assert_requests(state: &AppState, waits: usize, output: &GatewayOutput) {
    let diagnostics = diagnostics(output);
    assert_eq!(
        state.registrations.load(Ordering::SeqCst),
        1,
        "{diagnostics}"
    );
    assert_eq!(state.waits.load(Ordering::SeqCst), waits, "{diagnostics}");
    assert_eq!(
        state.unexpected_requests.load(Ordering::SeqCst),
        0,
        "{diagnostics}"
    );
}

fn assert_exit(output: &GatewayOutput, code: i32) {
    assert!(
        !output.timed_out,
        "gateway exceeded outer timeout; {}",
        diagnostics(output)
    );
    assert_eq!(output.status.code(), Some(code), "{}", diagnostics(output));
}

fn diagnostics(output: &GatewayOutput) -> String {
    format!(
        "status={}\nstdout:\n{}\nstderr:\n{}",
        output.status, output.stdout, output.stderr
    )
}

#[tokio::test]
async fn tool_approval_explicit_denial_preserves_operator_reason() {
    let server = TestServer::start(Scenario::ApprovalDenied).await;
    let output = run_gateway(&server, "approval", &approval_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 0);
    assert_eq!(
        output.stdout,
        r#"{"allowed":false,"reason":"operator denied this"}"#,
        "{}",
        diagnostics(&output)
    );
    assert_requests(&state, 1, &output);
}

#[tokio::test]
async fn tool_approval_healthy_wait_timeout_is_neutral_operator_away_response() {
    let server = TestServer::start(Scenario::ApprovalTimeout).await;
    let output = run_gateway(&server, "approval", &approval_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 0);
    assert_eq!(
        output.stdout,
        format!(r#"{{"allowed":false,"reason":"{TIMEOUT_REASON}"}}"#),
        "{}",
        diagnostics(&output)
    );
    assert!(
        !output.stdout.contains("pipeline"),
        "{}",
        diagnostics(&output)
    );
    assert!(
        !output.stdout.contains("operator denied"),
        "{}",
        diagnostics(&output)
    );
    assert_requests(&state, 2, &output);
}

#[tokio::test]
async fn tool_approval_failed_wait_until_deadline_is_neutral_pipeline_response() {
    let server = TestServer::start(Scenario::ApprovalPipelineError).await;
    let output = run_gateway(&server, "approval", &approval_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 0);
    assert_eq!(
        output.stdout,
        format!(r#"{{"allowed":false,"reason":"{PIPELINE_REASON}"}}"#),
        "{}",
        diagnostics(&output)
    );
    assert!(
        !output.stdout.contains("timed out"),
        "{}",
        diagnostics(&output)
    );
    assert!(
        !output.stdout.contains("operator denied"),
        "{}",
        diagnostics(&output)
    );
    assert_requests(&state, 2, &output);
}

#[tokio::test]
async fn question_explicit_rejection_exits_one() {
    let server = TestServer::start(Scenario::QuestionRejected).await;
    let output = run_gateway(&server, "question", &question_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 1);
    assert!(output.stdout.is_empty(), "{}", diagnostics(&output));
    assert_requests(&state, 1, &output);
}

#[tokio::test]
async fn question_healthy_wait_timeout_exits_three() {
    let server = TestServer::start(Scenario::QuestionTimeout).await;
    let output = run_gateway(&server, "question", &question_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 3);
    assert!(output.stdout.is_empty(), "{}", diagnostics(&output));
    assert_requests(&state, 2, &output);
}

#[tokio::test]
async fn question_failed_wait_until_deadline_exits_four() {
    let server = TestServer::start(Scenario::QuestionPipelineError).await;
    let output = run_gateway(&server, "question", &question_input()).await;
    let state = server.state.clone();
    server.stop().await;

    assert_exit(&output, 4);
    assert!(output.stdout.is_empty(), "{}", diagnostics(&output));
    assert_requests(&state, 2, &output);
}

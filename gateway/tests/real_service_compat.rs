use std::process::Stdio;
use std::time::Duration;

use agent_hub_approval_client::ApprovalClient;
use agent_hub_approval_service::config::{ApprovalFeatureMode, AuthMode, ServerConfig, Token};
use agent_hub_approval_service::notifier::NullNotifier;
use agent_hub_approval_service::sessions::SessionApprovalMode;
use agent_hub_approval_service::{AppState, router};
use protocol::{
    ApprovalDecision, ApprovalResolveRequest, QuestionDecision, QuestionResolveRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn start_server() -> (String, tokio::task::JoinHandle<std::io::Result<()>>) {
    let state = AppState::new(
        ServerConfig {
            auth_mode: AuthMode::None,
            tokens: vec![],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Readwrite,
            base_url: Some("http://127.0.0.1".into()),
            default_approval_mode: SessionApprovalMode::Remote,
        },
        NullNotifier,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router(state)).await });
    (format!("http://{address}"), task)
}

async fn start_authenticated_api_server() -> (String, tokio::task::JoinHandle<std::io::Result<()>>)
{
    let state = AppState::new(
        ServerConfig {
            auth_mode: AuthMode::Token,
            tokens: vec![Token {
                label: "compatibility".into(),
                secret: protocol::Secret::new("compatibility-token"),
            }],
            listen_addr: "127.0.0.1:0".into(),
            presence_ttl_secs: 120,
            session_ttl_secs: 7200,
            notification_delay_secs: 0,
            approval_mode: ApprovalFeatureMode::Disabled,
            base_url: None,
            default_approval_mode: SessionApprovalMode::Remote,
        },
        NullNotifier,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router(state)).await });
    (format!("http://{address}"), task)
}

async fn run_gateway(server: &str, command: &str, input: &str) -> tokio::process::Child {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"));
    process
        .arg(command)
        .arg("--opencode")
        .arg("--server")
        .arg(server)
        .arg("--timeout")
        .arg("5");
    if command == "approval" {
        process
            .arg("--config")
            .arg("gateway/config/test-local.json");
    }
    let mut child = process
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .await
        .unwrap();
    child
}

async fn wait_for_pending_approval(client: &ApprovalClient) -> protocol::Approval {
    for _ in 0..100 {
        let pending = client.list_pending_approvals().await.unwrap().value;
        if let Some(approval) = pending.into_iter().next() {
            return approval;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("gateway did not register an approval");
}

async fn wait_for_pending_question(client: &ApprovalClient) -> protocol::PendingQuestion {
    for _ in 0..100 {
        let pending = client.list_pending_questions().await.unwrap().value;
        if let Some(question) = pending.into_iter().next() {
            return question;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("gateway did not register a question");
}

#[tokio::test]
#[ignore = "loopback and gateway process compatibility; run explicitly"]
async fn real_client_gateway_approval_question_and_mobile_queue_are_compatible() {
    let (server, server_task) = start_server().await;
    let client = ApprovalClient::new(&server, None).unwrap();

    let approval_input = serde_json::json!({
        "session_id": "compatibility-session",
        "session_title": "Compatibility",
        "tool_name": "mcp_release_check",
        "tool_input": {"value": 1},
        "cwd": "/workspace",
        "workspace_roots": ["/workspace"],
        "hook_event_name": "tool.execute.before"
    })
    .to_string();
    let approval_gateway = run_gateway(&server, "approval", &approval_input).await;
    let approval = wait_for_pending_approval(&client).await;
    client
        .resolve_approval(
            approval.id,
            &ApprovalResolveRequest {
                decision: ApprovalDecision::Approve,
                message: Some("compatible".into()),
            },
        )
        .await
        .unwrap();
    let approval_output = approval_gateway.wait_with_output().await.unwrap();
    assert!(approval_output.status.success());
    assert_eq!(
        String::from_utf8(approval_output.stdout).unwrap(),
        r#"{"allowed":true}"#
    );

    let question_input = serde_json::json!({
        "id": "compatibility-question",
        "session_id": "compatibility-session",
        "session_display_name": "Compatibility",
        "cwd": "/workspace",
        "question_request_id": "question-1",
        "questions": [{
            "question": "Continue?",
            "header": "Decision",
            "options": [{"label": "Yes", "description": "Continue"}]
        }],
        "provider": "opencode"
    })
    .to_string();
    let question_gateway = run_gateway(&server, "question", &question_input).await;
    let question = wait_for_pending_question(&client).await;
    client
        .resolve_question(
            question.id,
            &QuestionResolveRequest {
                decision: QuestionDecision::Answer,
                answers: Some(vec![vec!["Yes".into()]]),
                reason: None,
            },
        )
        .await
        .unwrap();
    let question_output = question_gateway.wait_with_output().await.unwrap();
    assert!(question_output.status.success());
    assert_eq!(
        String::from_utf8(question_output.stdout).unwrap(),
        r#"{"answers":[["Yes"]]}"#
    );

    let address = server.strip_prefix("http://").unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /approvals/queue HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 200")
    );

    server_task.abort();
    let _ = server_task.await;

    let (authenticated_server, authenticated_task) = start_authenticated_api_server().await;
    let authenticated_client = ApprovalClient::new(
        &authenticated_server,
        Some(protocol::Secret::new("compatibility-token")),
    )
    .unwrap();
    assert_eq!(authenticated_client.session_status().await.unwrap(), 200);
    assert_eq!(
        ApprovalClient::new(&authenticated_server, None)
            .unwrap()
            .session_status()
            .await
            .unwrap(),
        401
    );
    authenticated_task.abort();
    let _ = authenticated_task.await;
}

#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use agent_hub_approval_client::ApprovalClient;
use protocol::{
    ApprovalContext, ApprovalDecision, ApprovalRequest, ApprovalResolveRequest, ApprovalStatus,
    QuestionDecision, QuestionInfo, QuestionOption, QuestionProxyRequest, QuestionResolveRequest,
    QuestionStatus, RequestType, SessionId,
};
use uuid::Uuid;

fn packaged_environment(command: &mut Command, listen_addr: &str) {
    for line in include_str!("../../deploy/approvals-server/approvals-server.env").lines() {
        let (key, value) = line.split_once('=').unwrap();
        command.env(key, value);
    }
    command
        .env("LISTEN_ADDR", listen_addr)
        .env("BASE_URL", format!("http://{listen_addr}"));
}

fn start_server(state_path: &std::path::Path, listen_addr: &str) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-hub-approvals-server"));
    command
        .args([
            "serve",
            "noop",
            "local-file",
            "--path",
            state_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    packaged_environment(&mut command, listen_addr);
    command.spawn().unwrap()
}

async fn wait_until_ready(child: &mut Child, client: &ApprovalClient) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited before readiness"
        );
        if matches!(client.health().await, Ok(200)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "server readiness timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn kill_9(child: &mut Child) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(!child.wait().unwrap().success());
}

fn request(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        id: id.into(),
        request_digest: None,
        session_id: SessionId::new("deployment-session"),
        session_display_name: "Deployment integration".into(),
        cwd: "/workspace/deployment".into(),
        tool: serde_json::from_value(serde_json::json!("Bash")).unwrap(),
        tool_input: serde_json::json!({"command": "cargo test"}),
        provider: "opencode".into(),
        request_type: RequestType::ToolUse,
        context: ApprovalContext {
            workspace_roots: vec!["/workspace/deployment".into()],
            hook_event_name: "permission.ask".to_string().into(),
            extra: None,
            attended: None,
        },
    }
}

fn question(id: &str) -> QuestionProxyRequest {
    QuestionProxyRequest {
        id: id.into(),
        session_id: SessionId::new("deployment-question-session"),
        session_display_name: "Deployment question integration".into(),
        cwd: "/workspace/deployment".into(),
        question_request_id: "opencode-question".into(),
        questions: vec![QuestionInfo {
            question: "Continue?".into(),
            header: "Decision".into(),
            options: vec![QuestionOption {
                label: "Yes".into(),
                description: "Continue".into(),
            }],
            multiple: Some(false),
            custom: Some(true),
        }],
        provider: "opencode".into(),
    }
}

#[tokio::test]
async fn packaged_server_real_client_lifecycle_is_kill_9_durable() {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("approval-crash-durability-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let state_path = directory.join("server_data.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen_addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let client = ApprovalClient::new(&format!("http://{listen_addr}"), None).unwrap();

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    let pending_request = request("kill9-pending");
    let pending = client
        .register_approval(&pending_request, Some(Uuid::new_v4()), None)
        .await
        .unwrap()
        .value;
    kill_9(&mut child);

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    let retry = client
        .register_approval(&pending_request, Some(Uuid::new_v4()), None)
        .await
        .unwrap()
        .value;
    assert_eq!(retry.id, pending.id);
    assert_eq!(retry.status, ApprovalStatus::Pending);
    client
        .resolve_approval(
            pending.id,
            &ApprovalResolveRequest {
                decision: ApprovalDecision::Approve,
                message: Some("approved".into()),
            },
        )
        .await
        .unwrap();
    kill_9(&mut child);

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    let resolved = client.get_approval(pending.id).await.unwrap().value;
    assert_eq!(
        resolved.status,
        ApprovalStatus::Approved {
            message: Some("approved".into())
        }
    );
    let resolved_retry = client
        .register_approval(&pending_request, None, None)
        .await
        .unwrap()
        .value;
    assert_eq!(resolved_retry.id, pending.id);
    assert!(resolved_retry.status.is_resolved());

    let pending_question_request = question("kill9-question");
    let pending_question = client
        .register_question(&pending_question_request)
        .await
        .unwrap()
        .value;
    kill_9(&mut child);

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    let question_retry = client
        .register_question(&pending_question_request)
        .await
        .unwrap()
        .value;
    assert_eq!(question_retry.id, pending_question.id);
    assert_eq!(question_retry.status, QuestionStatus::Pending);
    let mut conflicting_question = pending_question_request.clone();
    conflicting_question.cwd = "/workspace/different".into();
    assert_eq!(
        client
            .register_question(&conflicting_question)
            .await
            .unwrap_err()
            .status(),
        Some(409)
    );
    client
        .resolve_question(
            pending_question.id,
            &QuestionResolveRequest {
                decision: QuestionDecision::Answer,
                answers: Some(vec![vec!["Yes".into()]]),
                reason: None,
            },
        )
        .await
        .unwrap();
    kill_9(&mut child);

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    assert_eq!(
        client
            .get_question(pending_question.id)
            .await
            .unwrap()
            .value
            .status,
        QuestionStatus::Answered {
            answers: vec![vec!["Yes".into()]]
        }
    );
    let resolved_question_retry = client
        .register_question(&pending_question_request)
        .await
        .unwrap()
        .value;
    assert_eq!(resolved_question_retry.id, pending_question.id);
    assert!(resolved_question_retry.status.is_resolved());

    let cancel_waiter = Uuid::new_v4();
    let cancelled = client
        .register_approval(&request("kill9-cancelled"), Some(cancel_waiter), None)
        .await
        .unwrap()
        .value;
    let cancel_response = client
        .cancel_approval_waiter(cancelled.id, cancel_waiter)
        .await
        .unwrap()
        .value;
    assert_eq!(cancel_response.status, ApprovalStatus::Cancelled);
    kill_9(&mut child);

    let mut child = start_server(&state_path, &listen_addr);
    wait_until_ready(&mut child, &client).await;
    assert_eq!(
        client
            .get_approval(cancelled.id)
            .await
            .unwrap()
            .value
            .status,
        ApprovalStatus::Cancelled
    );

    let wait_id = Uuid::new_v4();
    let waiting = client
        .register_approval(&request("gateway-wait-resolve"), Some(wait_id), None)
        .await
        .unwrap()
        .value;
    let waiting_id = waiting.id;
    let waiting_client = client.clone();
    let wait = tokio::spawn(async move {
        waiting_client
            .wait_approval(waiting_id, Some(wait_id), Duration::from_secs(3))
            .await
            .unwrap()
            .value
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    client
        .resolve_approval(
            waiting_id,
            &ApprovalResolveRequest {
                decision: ApprovalDecision::Deny,
                message: Some("denied".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        wait.await.unwrap().status,
        ApprovalStatus::Denied {
            reason: "denied".into()
        }
    );

    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    assert!(child.wait().unwrap().success());
    std::fs::remove_dir_all(directory).unwrap();
}

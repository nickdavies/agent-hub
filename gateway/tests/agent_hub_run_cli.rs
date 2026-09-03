use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionAuthority, AgentHubRunDecisionV1,
    AgentHubRunResolvedV1, AttendedAuthorizationArtifact,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn request() -> AgentHubRunResolvedV1 {
    let artifact: AttendedAuthorizationArtifact =
        serde_json::from_slice(include_bytes!("attended-v3-artifact.json")).unwrap();
    let requested_at = Utc::now();
    AgentHubRunResolvedV1::new(
        Uuid::new_v4().to_string(),
        artifact,
        requested_at,
        requested_at + chrono::Duration::minutes(5),
    )
    .unwrap()
}

fn invoke(config: &std::path::Path, server: &str, input: &Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
        .env_clear()
        .args([
            "agent-hub-run-resolved",
            "--server",
            server,
            "--config",
            config.to_str().unwrap(),
            "--timeout",
            "2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(input).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn config(dir: &tempfile::TempDir, action: &str) -> std::path::PathBuf {
    let path = dir.path().join(format!("{action}.json"));
    std::fs::write(
        &path,
        json!({
            "version": 1,
            "default": "deny",
            "rules": [{
                "tools": ["agent_hub_run_resolved"],
                "input": {
                    "schema": "agent_hub_run_resolved_v1",
                    "canonical_execution": {"version": 3}
                },
                "action": action
            }]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn read_http_body(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap();
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes[header_end..header_end + content_length].to_vec()
}

fn respond(stream: &mut TcpStream, body: &Value) {
    let body = serde_json::to_vec(body).unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();
}

#[test]
fn structured_policy_allow_and_deny_return_bound_terminal_decisions() {
    let dir = tempfile::tempdir().unwrap();
    for (action, expected) in [
        ("allow", AgentHubRunDecision::Allow),
        ("deny", AgentHubRunDecision::Deny),
    ] {
        let request = request();
        let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
        let output = invoke(&config(&dir, action), "http://127.0.0.1:9", &hook);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let decision: AgentHubRunDecisionV1 = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decision.decision, expected);
        assert_eq!(decision.authority, AgentHubRunDecisionAuthority::Automatic);
        assert_eq!(decision.approval_id, request.invocation_id);
        decision.validate(&request, Utc::now()).unwrap();
    }
}

#[test]
fn ask_retries_identical_registration_and_preserves_wire_bindings() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let approval_id = Uuid::new_v4();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bodies);
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            captured.lock().unwrap().push(read_http_body(&mut stream));
            respond(
                &mut stream,
                &json!({"id": approval_id, "type": "approved", "message": "reviewed"}),
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let request = request();
    let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    for _ in 0..2 {
        let output = invoke(&config(&dir, "ask"), &format!("http://{address}"), &hook);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let decision: AgentHubRunDecisionV1 = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decision.authority, AgentHubRunDecisionAuthority::Human);
        assert_eq!(decision.approval_id, approval_id.to_string());
        decision.validate(&request, Utc::now()).unwrap();
    }
    server.join().unwrap();
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies[0], bodies[1]);
    let approval: protocol::ApprovalRequest = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(approval.id, request.invocation_id);
    assert_eq!(
        approval.request_digest.as_deref(),
        Some(request.canonical_digest.as_str())
    );
    assert_eq!(
        approval.tool_input["request_digest"],
        request.request_digest
    );
    assert_eq!(
        approval.tool_input["requested_at"],
        serde_json::to_value(request.requested_at).unwrap()
    );
}

#[test]
fn wrong_and_expired_bindings_fail_before_policy_decision() {
    let dir = tempfile::tempdir().unwrap();
    let allow = config(&dir, "allow");
    let request = request();
    let mut wrong = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    wrong["session_id"] = "018f0f21-76c0-7b37-a446-31f82a5e7601".into();
    let output = invoke(&allow, "http://127.0.0.1:9", &wrong);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let mut expired = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    expired["tool_input"]["expires_at"] = "2000-01-01T00:00:00Z".into();
    expired["tool_match_input"] = expired["tool_input"].clone();
    let output = invoke(&allow, "http://127.0.0.1:9", &expired);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

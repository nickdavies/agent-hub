use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionAuthority, AgentHubRunDecisionV1,
    AgentHubRunResolvedV1, AttendedAuthorizationRequestV3,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn request() -> AgentHubRunResolvedV1 {
    let request: AttendedAuthorizationRequestV3 = serde_json::from_slice(include_bytes!(
        "../../protocol/tests/fixtures/attended_authorization_v3.json"
    ))
    .unwrap();
    let requested_at = Utc::now();
    AgentHubRunResolvedV1::new(
        Uuid::new_v4().to_string(),
        request.canonical_artifact().unwrap(),
        requested_at,
        requested_at + chrono::Duration::minutes(5),
    )
    .unwrap()
}

fn invoke(config: &std::path::Path, server: &str, input: &Value) -> Output {
    invoke_with_extra(config, server, input, &[])
}

fn invoke_with_extra(
    config: &std::path::Path,
    server: &str,
    input: &Value,
    extra: &[&str],
) -> Output {
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
        .args(extra)
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
                    "schema": "agent_hub_run_policy_projection_v1"
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

fn respond_status(stream: &mut TcpStream, status: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
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
    assert_eq!(approval.tool_input, serde_json::to_value(&request).unwrap());
}

#[test]
fn conflicting_registration_fails_closed_without_retrying_or_waiting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = Arc::new(Mutex::new(0));
    let captured = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    *captured.lock().unwrap() += 1;
                    read_http_body(&mut stream);
                    respond_status(&mut stream, "409 Conflict", "request conflict");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let request = request();
    let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    let output = invoke(&config(&dir, "ask"), &format!("http://{address}"), &hook);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("409 Conflict"));
    server.join().unwrap();
    assert_eq!(*requests.lock().unwrap(), 1);
}

#[test]
fn canonical_transport_is_compact_single_copy_and_within_budget() {
    let request = request();
    let hook = serde_json::to_vec(&request.gateway_event().unwrap()).unwrap();
    assert!(hook.len() <= protocol::AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES);
    let encoded = &request.canonical_execution.canonical_bytes_base64;
    let text = String::from_utf8(hook).unwrap();
    assert_eq!(text.matches(encoded).count(), 1);
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["tool_match_input"],
        serde_json::to_value(&request.policy_projection).unwrap()
    );
    assert!(
        value["tool_match_input"]
            .get("canonical_execution")
            .is_none()
    );
    let maximum_base64 = protocol::AGENT_HUB_RUN_MAX_CANONICAL_BYTES.div_ceil(3) * 4;
    assert!(
        maximum_base64 + 2 * protocol::AGENT_HUB_RUN_MAX_POLICY_PROJECTION_BYTES + 32 * 1024
            < protocol::AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES,
        "canonical, projection, and fixed envelope bounds must fit the 1 MiB wire budget"
    );
}

#[test]
fn policy_preview_returns_derived_projection_without_contacting_server() {
    let dir = tempfile::tempdir().unwrap();
    let request = request();
    let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    let output = invoke_with_extra(
        &config(&dir, "allow"),
        "http://127.0.0.1:9",
        &hook,
        &["--preview"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["schema"], "agent_hub_run_policy_preview_v1");
    assert_eq!(preview["matched_action"], "allow");
    assert_eq!(preview["matched_rule"]["index"], 0);
    assert_eq!(preview["matched_rule"]["action"], "allow");
    assert_eq!(
        preview["projection"],
        serde_json::to_value(request.policy_projection).unwrap()
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

    let mut wrong_projection = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    wrong_projection["tool_match_input"]["network_class"] = "none".into();
    let output = invoke(&allow, "http://127.0.0.1:9", &wrong_projection);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let mut expired = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    expired["tool_input"]["expires_at"] = "2000-01-01T00:00:00Z".into();
    let output = invoke(&allow, "http://127.0.0.1:9", &expired);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn registration_body_and_decision_reason_are_bounded_before_use() {
    let dir = tempfile::tempdir().unwrap();
    for (body, expected) in [
        (
            vec![b'x'; protocol::AGENT_HUB_RUN_MAX_GATEWAY_OUTPUT_BYTES + 1],
            "response exceeds limit",
        ),
        (
            serde_json::to_vec(&json!({
                "id": Uuid::new_v4(),
                "type": "denied",
                "reason": "x".repeat(protocol::AGENT_HUB_RUN_MAX_REASON_BYTES + 1)
            }))
            .unwrap(),
            "reason exceeds required bounds",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_body(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let request = request();
        let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
        let output = invoke(&config(&dir, "ask"), &format!("http://{address}"), &hook);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
        server.join().unwrap();
    }
}

#[test]
fn sigint_aborts_a_pending_registration() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_http_body(&mut stream);
        std::thread::sleep(Duration::from_secs(5));
    });
    let dir = tempfile::tempdir().unwrap();
    let request = request();
    let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    let server_url = format!("http://{address}");
    let config_path = config(&dir, "ask");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
        .env_clear()
        .args([
            "agent-hub-run-resolved",
            "--server",
            &server_url,
            "--config",
            config_path.to_str().unwrap(),
            "--timeout",
            "10",
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
        .write_all(&serde_json::to_vec(&hook).unwrap())
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("aborted by signal"));
    server.join().unwrap();
}

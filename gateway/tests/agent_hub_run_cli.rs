use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionAuthority, AgentHubRunDecisionV1,
    AgentHubRunResolvedV1, AttendedAuthorizationRequestV3, AttendedAuthorizationRequestV4,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn request() -> AgentHubRunResolvedV1 {
    let request: AttendedAuthorizationRequestV3 = serde_json::from_slice(include_bytes!(
        "../../protocol/tests/fixtures/attended_authorization_v3.json"
    ))
    .unwrap();
    let request = AttendedAuthorizationRequestV4::from(request);
    let requested_at = Utc::now();
    AgentHubRunResolvedV1::new(
        Uuid::new_v4().to_string(),
        request.canonical_artifact().unwrap(),
        requested_at,
        requested_at + chrono::Duration::minutes(5),
    )
    .unwrap()
}

#[test]
fn permission_scope_separates_capabilities_from_execution_audit_identity() {
    let original: AttendedAuthorizationRequestV3 = serde_json::from_slice(include_bytes!(
        "../../protocol/tests/fixtures/attended_authorization_v3.json"
    ))
    .unwrap();
    let original = AttendedAuthorizationRequestV4::from(original);
    let now = Utc::now();
    let resolved = |execution: AttendedAuthorizationRequestV4| {
        AgentHubRunResolvedV1::new(
            Uuid::new_v4().to_string(),
            execution.canonical_artifact().unwrap(),
            now,
            now + chrono::Duration::minutes(5),
        )
        .unwrap()
    };
    let first = resolved(original.clone());
    let mut rebuilt = original;
    rebuilt.proposal.image_id = format!("sha256:{}", "b".repeat(64));
    rebuilt.execution.config_digest = "implementation-build-b".into();
    for provider in &mut rebuilt.execution.credential_providers {
        provider.source = "/different/trusted/provider/location".into();
        provider.command = Some("/different/trusted/provider".into());
    }
    let second = resolved(rebuilt);
    assert_ne!(first.canonical_digest, second.canonical_digest);
    assert_eq!(
        first.permission_scope_digest().unwrap(),
        second.permission_scope_digest().unwrap()
    );

    let scope = first.policy_projection.clone();
    let scope_json = serde_json::to_string(&scope).unwrap();
    assert!(!scope_json.contains(&first.canonical_execution.digest));
    assert!(!scope_json.contains(&format!("sha256:{}", "a".repeat(64))));
    assert!(!scope_json.contains("config-digest"));
    let baseline = scope.permission_scope_digest().unwrap();
    let mut network = scope.clone();
    network
        .effective_network_grants
        .iter_mut()
        .find(|grant| grant.resource == protocol::NetworkResourceKind::NpmRegistry)
        .unwrap()
        .hosts
        .push("registry.yarnpkg.com".into());
    assert_ne!(baseline, network.permission_scope_digest().unwrap());
    let mut mount = scope.clone();
    mount
        .workspaces
        .iter_mut()
        .find(|item| item.access == protocol::WorkspaceAccess::ReadWrite)
        .unwrap()
        .access = protocol::WorkspaceAccess::ReadOnly;
    assert_ne!(baseline, mount.permission_scope_digest().unwrap());
    let mut capability = scope.clone();
    capability
        .capabilities
        .push(protocol::AgentHubRunCapabilityProjectionV1 {
            name: "repository-token".into(),
            access: "read_only".into(),
            targets: vec!["owner/repository".into()],
            constraints: Default::default(),
        });
    assert_ne!(baseline, capability.permission_scope_digest().unwrap());
    let mut limits = scope;
    limits.output_limits.stdout_bytes += 1;
    assert_ne!(baseline, limits.permission_scope_digest().unwrap());
    let mut resource_limits = first.policy_projection.clone();
    resource_limits.pids_limit += 1;
    resource_limits.cpu_limit = Some("1.5".into());
    assert_ne!(baseline, resource_limits.permission_scope_digest().unwrap());
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
                    "schema": "agent_hub_run_policy_projection_v2"
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
    read_http_request(stream).1
}

fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
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
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        headers,
        bytes[header_end..header_end + content_length].to_vec(),
    )
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
#[ignore = "loopback network integration; run explicitly with --ignored"]
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
    let scope_digest = request.permission_scope_digest().unwrap();
    assert_eq!(
        approval.request_digest.as_deref(),
        Some(scope_digest.as_str())
    );
    assert_eq!(approval.tool_input, serde_json::to_value(&request).unwrap());
}

#[test]
#[ignore = "loopback network integration; run explicitly with --ignored"]
fn conflicting_registration_fails_closed_without_retrying_or_waiting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(0));
    let captured = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let (request, _) = read_http_request(&mut stream);
            if request.starts_with("GET /shutdown ") {
                break;
            }
            *captured.lock().unwrap() += 1;
            respond_status(&mut stream, "409 Conflict", "request conflict");
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let request = request();
    let hook = serde_json::to_value(request.gateway_event().unwrap()).unwrap();
    let output = invoke(&config(&dir, "ask"), &format!("http://{address}"), &hook);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("409 Conflict"));
    let mut shutdown = TcpStream::connect(address).unwrap();
    shutdown
        .write_all(b"GET /shutdown HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\n\r\n")
        .unwrap();
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
#[ignore = "loopback network integration; run explicitly with --ignored"]
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
#[ignore = "loopback network integration; run explicitly with --ignored"]
fn sigint_unregisters_the_waiter_and_awaits_server_confirmation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let approval_id = Uuid::new_v4();
    let (wait_entered_tx, wait_entered_rx) = std::sync::mpsc::sync_channel(0);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (registration, _) = read_http_request(&mut stream);
        let waiter_header = registration
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("x-agent-hub-waiter-id: ")
                    .map(str::to_owned)
            })
            .expect("registration waiter header");
        Uuid::parse_str(&waiter_header).unwrap();
        respond(&mut stream, &json!({"id": approval_id, "type": "pending"}));

        let (mut wait_stream, _) = listener.accept().unwrap();
        let (wait, _) = read_http_request(&mut wait_stream);
        assert!(wait.starts_with(&format!(
            "GET /api/v1/approvals/{approval_id}/wait?waiter_id={waiter_header} "
        )));
        wait_entered_tx.send(()).unwrap();

        let (mut cancel_stream, _) = listener.accept().unwrap();
        let (cancel, body) = read_http_request(&mut cancel_stream);
        assert!(cancel.starts_with(&format!(
            "POST /api/v1/approvals/{approval_id}/waiters/cancel "
        )));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["waiter_id"],
            waiter_header
        );
        respond(
            &mut cancel_stream,
            &json!({"id": approval_id, "type": "cancelled"}),
        );
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
    wait_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("gateway entered wait");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("aborted by signal"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("server confirmed cancellation"));
    server.join().unwrap();
}

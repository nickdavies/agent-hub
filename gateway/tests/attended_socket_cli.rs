#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

use protocol::{AttendedGatewayDecisionKind, AttendedGatewayRequestV1};
use serde_json::Value;
use uuid::Uuid;

#[test]
fn real_gateway_cli_exchanges_one_framed_relay_decision() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("approval.sock");
    let config = dir.path().join("permissions.json");
    std::fs::write(&config, r#"{"version":1,"default":"ask","rules":[]}"#).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let relay = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).unwrap();
        let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
        stream.read_exact(&mut body).unwrap();
        let request: AttendedGatewayRequestV1 = serde_json::from_slice(&body).unwrap();
        request
            .validate(&request.binding, chrono::Utc::now())
            .unwrap();
        let decision = request.decision(AttendedGatewayDecisionKind::Allow, None);
        let frame = protocol::encode_attended_gateway_frame(&decision).unwrap();
        stream.write_all(&frame).unwrap();
    });

    let binding = serde_json::json!({
        "run_epoch": "018f0f21-76c0-7b37-a446-31f82a5e7601",
        "authorization_digest": "a".repeat(64),
        "environment_bundle_digest": "b".repeat(64),
        "image_digest": format!("sha256:{}", "c".repeat(64)),
        "network_digest": "d".repeat(64),
        "approval_bundle_digest": "e".repeat(64),
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
        .env_clear()
        .args([
            "approval",
            "--opencode",
            "--approval-mode",
            "attended_socket",
            "--approval-socket",
            socket.to_str().unwrap(),
            "--approval-binding",
            &serde_json::to_string(&binding).unwrap(),
            "--approval-deadline",
            "2099-01-02T03:04:05Z",
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
        .write_all(
            br#"{"session_id":"golden","tool_name":"custom_tool","tool_input":{"opaque":true},"cwd":"/workspace"}"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    relay.join().unwrap();
    assert!(
        output.status.success(),
        "gateway failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        serde_json::json!({"allowed": true})
    );
}

#[test]
fn attended_gateway_rejects_unknown_permission_fields_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let binding = serde_json::json!({
        "run_epoch": "018f0f21-76c0-7b37-a446-31f82a5e7601",
        "authorization_digest": "a".repeat(64),
        "environment_bundle_digest": "b".repeat(64),
        "image_digest": format!("sha256:{}", "c".repeat(64)),
        "network_digest": "d".repeat(64),
        "approval_bundle_digest": "e".repeat(64),
    });
    for contents in [
        r#"{"version":1,"default":"ask","rules":[],"unexpected":true}"#,
        r#"{"version":2,"default":"ask","rules":[]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["Bash"],"action":"ask","unexpected":true}]}"#,
    ] {
        let config = dir.path().join(Uuid::new_v4().to_string());
        std::fs::write(&config, contents).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
            .env_clear()
            .args([
                "approval",
                "--opencode",
                "--approval-mode",
                "attended_socket",
                "--approval-socket",
                "/missing/approval.sock",
                "--approval-binding",
                &serde_json::to_string(&binding).unwrap(),
                "--approval-deadline",
                "2099-01-02T03:04:05Z",
                "--config",
                config.to_str().unwrap(),
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
            .write_all(br#"{"session_id":"strict","tool_name":"custom","tool_input":{},"cwd":"/workspace"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unknown field") || stderr.contains("unsupported version"));
    }
}

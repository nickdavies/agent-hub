#![cfg(unix)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::Value;
use uuid::Uuid;

fn run_host_custom_tool(config: &std::path::Path, tool_input: &Value) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
        .env_clear()
        .args([
            "approval",
            "--opencode",
            "--server",
            "http://127.0.0.1:1",
            "--config",
            config.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::json!({
        "session_id": "custom-input",
        "tool_name": "custom_tool",
        "tool_input": tool_input,
        "cwd": "/workspace/project"
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&payload).unwrap().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn opencode_custom_tool_raw_input_reaches_structural_matcher() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("permissions.json");
    std::fs::write(
        &config,
        r#"{
            "version": 1,
            "default": "deny",
            "rules": [{
                "tools": ["custom_tool"],
                "in_cwds": ["/workspace/project"],
                "input": {"request": {"mode": "safe"}, "targets": ["one", "two"]},
                "action": "allow"
            }]
        }"#,
    )
    .unwrap();

    let matching = run_host_custom_tool(
        &config,
        &serde_json::json!({
            "targets": ["one", "two"],
            "request": {"trace": true, "mode": "safe"},
            "runtime_extra": true
        }),
    );
    assert!(
        matching.status.success(),
        "gateway failed: {}",
        String::from_utf8_lossy(&matching.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&matching.stdout).unwrap(),
        serde_json::json!({"allowed": true})
    );

    let mismatching = run_host_custom_tool(
        &config,
        &serde_json::json!({
            "targets": ["two", "one"],
            "request": {"mode": "safe"}
        }),
    );
    assert!(mismatching.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&mismatching.stdout).unwrap(),
        serde_json::json!({"allowed": false, "reason": "denied by policy"})
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
    let raw_digest = protocol::opencode_raw_args_digest(&serde_json::json!({})).unwrap();
    for contents in [
        r#"{"version":1,"default":"ask","rules":[],"unexpected":true}"#,
        r#"{"version":2,"default":"ask","rules":[]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["Bash"],"action":"ask","unexpected":true}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["Bash"],"action":"allow","command":"true"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["Bash"],"action":"ask","message":"bad"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["Bash"],"action":"delegate"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["custom"],"input":[],"action":"allow"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["custom"],"input":"value","action":"allow"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["custom"],"input":null,"action":"allow"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["custom"],"in_cwds":[],"action":"allow"}]}"#,
        r#"{"version":1,"default":"ask","rules":[{"tools":["custom"],"in_cwds":[""],"action":"allow"}]}"#,
    ] {
        let config = dir.path().join(Uuid::new_v4().to_string());
        let request_id = Uuid::new_v4().to_string();
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
                "--call-id",
                "call-strict",
                "--request-id",
                &request_id,
                "--raw-args-digest",
                &raw_digest,
                "--canonical-args-digest",
                &"b".repeat(64),
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
        assert!(
            stderr.contains("unknown field")
                || stderr.contains("unsupported version")
                || stderr.contains("invalid action/command")
                || stderr.contains("message outside")
                || stderr.contains("invalid 'in_cwds'")
                || stderr.contains("in_cwds roots")
                || stderr.contains("invalid type"),
            "unexpected validation error: {stderr}"
        );
    }
}

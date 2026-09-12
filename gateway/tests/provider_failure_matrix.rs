use std::process::{Command, Output, Stdio};

use serde_json::Value;

#[derive(Clone, Copy)]
struct ProviderFixture {
    flag: &'static str,
    payload: &'static str,
}

const PROVIDERS: [ProviderFixture; 3] = [
    ProviderFixture {
        flag: "--opencode",
        payload: r#"{"session_id":"session","tool_name":"bash","tool_input":{"command":"true"},"cwd":"/workspace","hook_event_name":"tool.execute.before"}"#,
    },
    ProviderFixture {
        flag: "--claude",
        payload: r#"{"session_id":"session","tool_name":"Bash","tool_input":{"command":"true"},"cwd":"/workspace","hook_event_name":"PreToolUse"}"#,
    },
    ProviderFixture {
        flag: "--cursor",
        payload: r#"{"conversation_id":"session","tool_name":"Shell","tool_input":{"command":"true"},"cwd":"/workspace","hook_event_name":"preToolUse"}"#,
    },
];

fn config(root: &std::path::Path, name: &str, action: &str, command: Option<&str>) -> String {
    let path = root.join(name);
    let rule = command.map(|command| {
        serde_json::json!({"tools":["Bash", "Shell"], "action":"delegate", "command":command})
    });
    std::fs::write(
        &path,
        serde_json::json!({
            "version": 1,
            "default": action,
            "rules": rule.into_iter().collect::<Vec<_>>(),
        })
        .to_string(),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn install_gateway(root: &std::path::Path) -> std::path::PathBuf {
    let bin = root.join("installed/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gateway = bin.join("agent-hub-gateway");
    std::fs::copy(env!("CARGO_BIN_EXE_agent-hub-gateway"), &gateway).unwrap();
    gateway
}

fn run(binary: &std::path::Path, fixture: ProviderFixture, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(binary)
        .arg("approval")
        .arg(fixture.flag)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_native_denial(fixture: ProviderFixture, output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stderr.len() <= 4096,
        "stderr was {} bytes",
        output.stderr.len()
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    match fixture.flag {
        "--opencode" => {
            assert_eq!(value["allowed"], false);
            assert!(
                value["reason"]
                    .as_str()
                    .unwrap()
                    .contains("Agent Hub denied")
            );
            assert!(value["reason"].as_str().unwrap().len() <= 4096);
        }
        "--claude" => {
            assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
            assert!(
                value["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains("Agent Hub denied")
            );
            assert!(
                value["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .len()
                    <= 4096
            );
        }
        "--cursor" => {
            assert_eq!(value["permission"], "deny");
            assert!(
                value["agent_message"]
                    .as_str()
                    .unwrap()
                    .contains("Agent Hub denied")
            );
            assert!(value["agent_message"].as_str().unwrap().len() <= 4096);
        }
        _ => unreachable!(),
    }
}

#[test]
fn installed_gateway_emits_native_denials_for_post_parse_failures() {
    let root = tempfile::tempdir().unwrap();
    let gateway = install_gateway(root.path());
    let allow = config(root.path(), "allow.json", "allow", None);
    let ask = config(root.path(), "ask.json", "ask", None);
    let delegate = config(
        root.path(),
        "delegate.json",
        "deny",
        Some(&format!("agent-hub-missing-delegate-{}", "x".repeat(5000))),
    );
    let missing = root
        .path()
        .join("missing.json")
        .to_string_lossy()
        .into_owned();
    let binding = serde_json::json!({
        "run_epoch": "018f0f21-76c0-7b37-a446-31f82a5e7601",
        "authorization_digest": "a".repeat(64),
        "environment_bundle_digest": "b".repeat(64),
        "image_digest": format!("sha256:{}", "c".repeat(64)),
        "network_digest": "d".repeat(64),
        "approval_bundle_digest": "e".repeat(64),
    })
    .to_string();
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_server = format!("http://{}", unavailable.local_addr().unwrap());
    drop(unavailable);

    for fixture in PROVIDERS {
        for args in [
            vec!["--server", "file:///invalid", "--config", &allow],
            vec!["--server", "http://0.0.0.0:1", "--config", &missing],
            vec!["--server", "http://0.0.0.0:1", "--config", &delegate],
            vec!["--approval-mode", "attended_socket", "--config", &allow],
        ] {
            assert_native_denial(fixture, &run(&gateway, fixture, &args, fixture.payload));
        }
        assert_native_denial(
            fixture,
            &run(
                &gateway,
                fixture,
                &["--server", &unavailable_server, "--config", &ask],
                fixture.payload,
            ),
        );

        let raw_args: Value =
            serde_json::from_str::<Value>(fixture.payload).unwrap()["tool_input"].clone();
        let raw_digest = protocol::opencode_raw_args_digest(&raw_args).unwrap();
        let transition_args = vec![
            "--approval-mode",
            "attended_socket",
            "--approval-socket",
            "/agent-hub/nonexistent/approval.sock",
            "--approval-binding",
            &binding,
            "--approval-deadline",
            "2099-01-02T03:04:05Z",
            "--call-id",
            "call-1",
            "--request-id",
            "018f0f21-76c0-7b37-a446-31f82a5e7602",
            "--raw-args-digest",
            &raw_digest,
            "--canonical-args-digest",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "--config",
            &allow,
        ];
        assert_native_denial(
            fixture,
            &run(&gateway, fixture, &transition_args, fixture.payload),
        );
    }
}

#[test]
fn installed_gateway_preparse_failure_contract_blocks_every_provider() {
    let root = tempfile::tempdir().unwrap();
    let gateway = install_gateway(root.path());
    for fixture in PROVIDERS {
        let output = run(
            &gateway,
            fixture,
            &["--server", "http://0.0.0.0:1"],
            "not-json",
        );
        assert!(output.stderr.len() <= 4096);
        if fixture.flag == "--claude" {
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
        } else {
            assert_native_denial(fixture, &output);
        }
    }

    let permission_request = run(
        &gateway,
        PROVIDERS[1],
        &["--server", "http://0.0.0.0:1"],
        r#"{"hook_event_name":"PermissionRequest"}"#,
    );
    assert!(permission_request.status.success());
    let value: Value = serde_json::from_slice(&permission_request.stdout).unwrap();
    assert_eq!(value["hookSpecificOutput"]["decision"]["behavior"], "deny");
}

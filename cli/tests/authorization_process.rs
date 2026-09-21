use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IMAGE_TAG: &str = "agent-hub/job-code:2026-09-09";
const QUALIFICATION_IMAGE_TAG: &str =
    "agent-hub/job-code-qualification:4d596913-2cc2-4068-a842-3db326aa6f1c";

#[test]
fn host_preview_requires_a_valid_host_home() {
    use std::os::unix::ffi::OsStringExt as _;
    for (home, error) in [
        (None, "HOME is required"),
        (Some("".into()), "trusted host HOME must be absolute"),
        (
            Some("relative".into()),
            "trusted host HOME must be absolute",
        ),
        (
            Some(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").into()),
            "trusted host HOME must be an existing directory",
        ),
        (
            Some(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
            "HOME is required and must be UTF-8",
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-hub"));
        command
            .env_clear()
            .args(["local", "preview", "--request-stdin"]);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(error));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn one_step_invalid_input_prints_a_bounded_failure_envelope_before_nonzero() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-hub"))
        .args([
            "local",
            "run",
            "--request-stdin",
            "--internal-invocation-id",
            "018f0f21-76c0-7b37-a446-31f82a5e7604",
        ])
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"{").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.len() <= protocol::AGENT_HUB_RUN_MAX_GATEWAY_INPUT_BYTES);
    let output: protocol::AgentHubRunOutputV1 = serde_json::from_slice(&output.stdout).unwrap();
    output.validate().unwrap();
    assert_eq!(
        output.approval_receipt.status,
        protocol::AgentHubRunApprovalReceiptStatus::Unavailable
    );
    assert_eq!(output.local_result_v2["status"], "failed");
    assert!(output.error.is_some());
}

#[test]
fn one_step_host_run_decides_before_side_effects_and_reuses_receipt_for_retry() {
    let root = temp_root();
    let config = root.join("home/config");
    let workspace = root.join("workspace");
    let state = root.join("state");
    let docker_marker = root.join("docker.calls");
    let fake_bin = root.join("bin");
    let gateway_script = fake_bin.join("agent-hub-gateway");
    let decision = root.join("gateway-decision");
    let export_root = workspace.join(".agent-hub/runs");
    std::fs::create_dir(&workspace).unwrap();
    initialize_repository(&workspace);
    std::fs::create_dir(&fake_bin).unwrap();
    write_config(&config, &workspace);
    for path in [
        config.join("environments/test/environment.toml"),
        config.join("images/job-code/image.toml"),
    ] {
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace(IMAGE_TAG, QUALIFICATION_IMAGE_TAG);
        std::fs::write(path, body).unwrap();
    }
    write_fake_docker(&fake_bin.join("docker"), &docker_marker, &workspace, &root);
    write_fake_run_gateway(&gateway_script, &decision, &state);
    let host_dir = root.join("home/.config/agent-hub");
    std::fs::create_dir_all(&host_dir).unwrap();
    let gateway_config = host_dir.join("permissions.json");
    std::fs::write(&gateway_config, "{}").unwrap();
    let plugin_install = host_dir.join("opencode-plugin.js");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_agent-hub"));
    let config_root = std::fs::canonicalize(&config).unwrap();
    std::fs::write(
        host_dir.join("host-run.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema":"agent_hub_run_host_v2",
            "config_root":config_root,
            "gateway":{"path":std::fs::canonicalize(&gateway_script).unwrap()},
            "policy":{"path":std::fs::canonicalize(&gateway_config).unwrap()},
            "managed_images":{"job-code":QUALIFICATION_IMAGE_TAG},
            "backend_metadata_revision":"integration-test-1"
        }))
        .unwrap(),
    )
    .unwrap();
    let host_config_path = host_dir.join("host-run.json");
    let baseline_host_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&host_config_path).unwrap()).unwrap();
    let launch = serde_json::json!({
        "schema":"agent_hub_run_intent_v1",
        "environment":"test",
        "image_family":"job-code",
        "network_class":"none",
        "network_resources":[],
        "tool_approval":"auto",
        "overlays":[],
        "timeout_secs":30,
        "prompt":"Return success",
        "workspaces":[{"alias":"primary","path":workspace,"access":"read_write"}],
        "context_roots":[],
        "primary_workspace":"primary",
        "workspace_export_root":export_root
    });
    let invocation_id = "018f0f21-76c0-7b37-a446-31f82a5e7600";
    let args = [
        "local",
        "run",
        "--request-stdin",
        "--internal-invocation-id",
        invocation_id,
    ];

    let alternate_host_config = host_dir.join("alternate-host-run.json");
    std::fs::copy(host_dir.join("host-run.json"), &alternate_host_config).unwrap();
    let untrusted = invoke(
        &root,
        &fake_bin,
        &state,
        &[
            "local",
            "run",
            "--request-stdin",
            "--host-config",
            alternate_host_config.to_str().unwrap(),
        ],
        Some(&launch.to_string()),
    );
    assert!(!untrusted.status.success());
    assert!(
        String::from_utf8_lossy(&untrusted.stderr).contains("trusted host configuration identity")
    );
    assert!(!state.exists());

    for contents in [
        None,
        Some("independent plugin release"),
        Some("replacement plugin release"),
    ] {
        if let Some(contents) = contents {
            let replacement = host_dir.join("replacement-plugin.js");
            std::fs::write(&replacement, contents).unwrap();
            std::fs::rename(replacement, &plugin_install).unwrap();
        }
        for operation in ["inspect", "preview"] {
            let output = invoke(
                &root,
                &fake_bin,
                &state,
                &["local", operation, "--request-stdin"],
                Some(&launch.to_string()),
            );
            assert_success(&output);
        }
        assert!(!state.join("runs").exists());
        assert!(!state.join("authorizations").exists());
        assert!(docker_marker.exists());
        std::fs::remove_file(&docker_marker).unwrap();
    }
    std::fs::remove_file(&plugin_install).unwrap();

    std::fs::write(&decision, "pending automatic").unwrap();
    let pending_invocation = "018f0f21-76c0-7b37-a446-31f82a5e7613";
    let mut pending = Command::new(env!("CARGO_BIN_EXE_agent-hub"));
    pending
        .args([
            "local",
            "run",
            "--request-stdin",
            "--internal-invocation-id",
            pending_invocation,
        ])
        .current_dir(&root)
        .env("AGENT_HUB_ATTENDED_STATE", &state)
        .env("HOME", root.join("home"))
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut pending = pending.spawn().unwrap();
    use std::io::Write as _;
    pending
        .stdin
        .take()
        .unwrap()
        .write_all(launch.to_string().as_bytes())
        .unwrap();
    wait_for_file(&root.join("gateway-entered"));
    assert_eq!(unsafe { libc::kill(pending.id() as i32, libc::SIGTERM) }, 0);
    let pending = pending.wait_with_output().unwrap();
    assert!(!pending.status.success());
    let output: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert_eq!(output["approval_receipt"]["status"], "unavailable");
    assert!(
        state
            .join(format!(
                "agent-hub-run/invocations/{pending_invocation}/request.json"
            ))
            .is_file()
    );
    assert!(
        !state
            .join(format!("authorizations/records/{pending_invocation}.json"))
            .exists()
    );
    assert!(
        std::fs::read_to_string(&docker_marker)
            .unwrap()
            .lines()
            .all(|line| line.starts_with("image inspect --format {{.Id}} "))
    );
    std::fs::remove_file(&docker_marker).unwrap();
    assert!(
        root.join("gateway-cancel-confirmed").is_file(),
        "host returned before the gateway confirmed waiter cancellation"
    );
    let gateway_pid = std::fs::read_to_string(root.join("gateway.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(unsafe { libc::kill(gateway_pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );

    for invalid_decision in ["allow automatic wrong", "allow automatic expired"] {
        std::fs::write(&decision, invalid_decision).unwrap();
        let rejected = invoke(&root, &fake_bin, &state, &args, Some(&launch.to_string()));
        assert!(!rejected.status.success());
        let failure: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(failure["schema"], "agent_hub_run_output_v1");
        assert_eq!(failure["approval_receipt"]["status"], "unavailable");
        assert_eq!(failure["local_result_v2"]["status"], "failed");
        assert!(!state.join("authorizations/records").exists());
        assert!(!state.join("runs").exists());
        assert!(
            std::fs::read_to_string(&docker_marker)
                .unwrap()
                .lines()
                .all(|line| line.starts_with("image inspect --format {{.Id}} "))
        );
        std::fs::remove_file(&docker_marker).unwrap();
    }

    std::fs::write(&decision, "deny automatic").unwrap();
    let denied = invoke(&root, &fake_bin, &state, &args, Some(&launch.to_string()));
    assert!(!denied.status.success());
    assert!(
        !denied.stdout.is_empty(),
        "one-step deny produced no receipt: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(
        std::fs::read_to_string(&docker_marker)
            .unwrap()
            .lines()
            .all(|line| line.starts_with("image inspect --format {{.Id}} ")),
        "denied one-step launch performed a Docker operation beyond managed image resolution"
    );
    std::fs::remove_file(&docker_marker).unwrap();
    let denied_json: serde_json::Value = serde_json::from_slice(&denied.stdout).unwrap();
    assert_eq!(
        denied_json["approval_receipt"]["receipt"]["decision"], "deny",
        "{denied_json:#}"
    );
    assert_eq!(
        denied_json["approval_receipt"]["receipt"]["authority"],
        "automatic"
    );
    assert!(state.join("agent-hub-run/invocations").is_dir());
    assert!(!state.join("authorizations/records").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "tracked\n"
    );
    let status = Command::new("git")
        .arg("-C")
        .arg(&workspace)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.status.success() && status.stdout.is_empty());

    let allowed_invocation_id = "018f0f21-76c0-7b37-a446-31f82a5e7602";
    let allowed_args = [
        "local",
        "run",
        "--request-stdin",
        "--internal-invocation-id",
        allowed_invocation_id,
    ];
    std::fs::write(&decision, "allow human").unwrap();
    std::fs::write(root.join("provider-503-once"), "fail").unwrap();
    let allowed = invoke(
        &root,
        &fake_bin,
        &state,
        &allowed_args,
        Some(&launch.to_string()),
    );
    assert_success(&allowed);
    let output: serde_json::Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert_eq!(output["approval_receipt"]["receipt"]["decision"], "allow");
    assert_eq!(output["approval_receipt"]["receipt"]["authority"], "human");
    assert_eq!(output["local_result_v2"]["status"], "succeeded");
    let run_id = output["local_result_v2"]["run_id"].as_str().unwrap();
    let export_run = export_root.join(run_id);
    assert_eq!(
        output["local_result_v2"]["paths"]["workspace_export"],
        export_run.to_str().unwrap()
    );
    assert!(export_run.join("manifest.json").is_file());
    assert!(docker_marker.exists());

    let invocation = output["invocation_id"].as_str().unwrap();
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join(format!("authorizations/records/{invocation}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(record["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(
        record["attempts"][0]["retry_class"],
        "provider_service_unavailable"
    );
    assert_eq!(record["attempts"][0]["retry_eligible"], true);
    let first_run_id = record["attempts"][0]["run_id"].as_str().unwrap();
    let first_run: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join(format!("executions/{first_run_id}.json"))).unwrap(),
    )
    .unwrap();
    let finished_at =
        chrono::DateTime::parse_from_rfc3339(first_run["finished_at"].as_str().unwrap())
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap();
    let cleanup_events = std::fs::read_to_string(root.join("cleanup.events")).unwrap();
    let first_container = first_run["container_name"].as_str().unwrap();
    let observed = cleanup_events
        .lines()
        .filter(|line| line.contains(first_container))
        .map(|line| line.split_once(' ').unwrap().0.parse::<i64>().unwrap())
        .collect::<Vec<_>>();
    assert!(!observed.is_empty());
    assert!(observed.into_iter().all(|event| event <= finished_at));
    assert_eq!(
        record["receipt_id"],
        output["approval_receipt"]["receipt"]["approval_id"]
    );
    assert_eq!(
        record["decision_receipt"],
        output["approval_receipt"]["receipt"]
    );
    let docker_calls = std::fs::read_to_string(&docker_marker).unwrap();
    assert!(
        docker_calls
            .lines()
            .any(|call| call.starts_with("image inspect "))
    );
    assert!(
        docker_calls
            .lines()
            .filter(|call| !call.starts_with("image inspect "))
            .all(|call| !call.contains(QUALIFICATION_IMAGE_TAG)),
        "mutable managed tag escaped image resolution into launch: {docker_calls}"
    );
    assert!(
        docker_calls.contains(&format!("sha256:{}", "a".repeat(64))),
        "captured immutable image ID was not used by Docker create: {docker_calls}"
    );
    assert!(
        !docker_calls.contains("agent-hub-job-code-preflight"),
        "attended startup still ran the comprehensive image preflight: {docker_calls}"
    );
    let repeated = invoke(
        &root,
        &fake_bin,
        &state,
        &allowed_args,
        Some(&launch.to_string()),
    );
    assert_success(&repeated);
    let repeated: serde_json::Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated["invocation_id"], output["invocation_id"]);

    for (invocation, signal) in [
        ("018f0f21-76c0-7b37-a446-31f82a5e7603", libc::SIGTERM),
        ("018f0f21-76c0-7b37-a446-31f82a5e7604", libc::SIGINT),
    ] {
        std::fs::write(root.join("slow-launch"), "slow").unwrap();
        let _ = std::fs::remove_file(root.join("docker-entered"));
        let mut cancelled = Command::new(env!("CARGO_BIN_EXE_agent-hub"));
        cancelled
            .args([
                "local",
                "run",
                "--request-stdin",
                "--internal-invocation-id",
                invocation,
            ])
            .current_dir(&root)
            .env("AGENT_HUB_ATTENDED_STATE", &state)
            .env("HOME", root.join("home"))
            .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut cancelled = cancelled.spawn().unwrap();
        use std::io::Write as _;
        cancelled
            .stdin
            .take()
            .unwrap()
            .write_all(launch.to_string().as_bytes())
            .unwrap();
        wait_for_file(&root.join("docker-entered"));
        assert_eq!(unsafe { libc::kill(cancelled.id() as i32, signal) }, 0);
        let cancelled = cancelled.wait_with_output().unwrap();
        assert!(
            !cancelled.status.success(),
            "cancelled one-step unexpectedly succeeded: {} {}",
            String::from_utf8_lossy(&cancelled.stdout),
            String::from_utf8_lossy(&cancelled.stderr)
        );
        let cancelled: serde_json::Value = serde_json::from_slice(&cancelled.stdout).unwrap();
        assert_eq!(cancelled["approval_receipt"]["status"], "issued");
        assert_eq!(
            cancelled["local_result_v2"]["execution_outcome"],
            "cancelled"
        );
        assert_eq!(cancelled["local_result_v2"]["cleanup_outcome"], "succeeded");
        let cancelled_run_id = cancelled["local_result_v2"]["run_id"].as_str().unwrap();
        assert!(
            state
                .join(format!("cancellations/{cancelled_run_id}.json"))
                .is_file(),
            "signal did not create a durable cancellation marker for the owned run"
        );
    }

    let built_gateway = executable.with_file_name("agent-hub-gateway");
    assert!(
        built_gateway.is_file(),
        "build the real gateway first: cargo build -p agent-hub-gateway"
    );
    // Cargo may hard-link top-level binaries to hashed build artifacts. Copy the
    // fixture executable so protected-path validation sees a deployment-like inode.
    let real_gateway = root.join("real-agent-hub-gateway");
    std::fs::copy(built_gateway, &real_gateway).unwrap();
    let policy = serde_json::json!({
        "version": 1,
        "default": "deny",
        "rules": [
            {"tools": ["internal_tool"], "in_cwds": ["~/config"], "action": "deny"},
            {"tools": ["agent_hub_run_resolved"], "in_cwds": ["~/config"],
             "input": {"timeout_secs": 31}, "action": "deny"},
            {"tools": ["agent_hub_run_resolved"], "in_cwds": ["~/config"], "action": "allow"}
        ]
    });
    let policy_bytes = serde_json::to_vec(&policy).unwrap();
    std::fs::write(&gateway_config, &policy_bytes).unwrap();
    let mut real_host_config = baseline_host_config.clone();
    real_host_config["gateway"] = serde_json::json!({"path": real_gateway});
    real_host_config["policy"] = serde_json::json!({"path": gateway_config});
    std::fs::write(
        &host_config_path,
        serde_json::to_vec(&real_host_config).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(root.join("slow-launch")).unwrap();
    for (timeout, expected) in [(30, "allow"), (31, "deny")] {
        let mut request = launch.clone();
        request["timeout_secs"] = timeout.into();
        let preview = invoke(
            &root,
            &fake_bin,
            &state,
            &["local", "preview", "--request-stdin"],
            Some(&request.to_string()),
        );
        assert_success(&preview);
        let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
        assert_eq!(preview["matched_action"], expected);
        let run = invoke(
            &root,
            &fake_bin,
            &state,
            &[
                "local",
                "run",
                "--request-stdin",
                "--internal-invocation-id",
                &uuid::Uuid::new_v4().to_string(),
            ],
            Some(&request.to_string()),
        );
        if expected == "allow" {
            assert_success(&run);
        } else {
            assert!(!run.status.success());
        }
        let run: serde_json::Value = serde_json::from_slice(&run.stdout).unwrap();
        assert_eq!(run["approval_receipt"]["receipt"]["decision"], expected);
        assert_eq!(run["approval_receipt"]["receipt"]["authority"], "automatic");
        assert_eq!(std::fs::read(&gateway_config).unwrap(), policy_bytes);
    }
    std::fs::write(&gateway_config, "{}").unwrap();
    std::fs::write(
        &host_config_path,
        serde_json::to_vec(&baseline_host_config).unwrap(),
    )
    .unwrap();

    let replacement = host_dir.join("replacement-policy.json");
    std::fs::write(&replacement, "{}").unwrap();
    std::fs::remove_file(&gateway_config).unwrap();
    std::os::unix::fs::symlink(&replacement, &gateway_config).unwrap();
    let replaced = invoke(
        &root,
        &fake_bin,
        &state,
        &allowed_args,
        Some(&launch.to_string()),
    );
    assert!(!replaced.status.success());
    let replaced: serde_json::Value = serde_json::from_slice(&replaced.stdout).unwrap();
    assert!(
        replaced["error"]
            .as_str()
            .unwrap()
            .contains("path must be canonical")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any())]
#[test]
fn split_launch_state_writes_recover_across_process_restarts() {
    for boundary in [
        "launch_pending_record_published",
        "consumption_lineage_published",
        "running_record_published",
        "terminal_record_published",
    ] {
        let root = temp_root();
        let config = root.join("config");
        let workspace = root.join("workspace");
        let state = root.join("state");
        let authorization = root.join("authorization.json");
        let fake_bin = root.join("bin");
        std::fs::create_dir(&workspace).unwrap();
        initialize_repository(&workspace);
        std::fs::create_dir(&fake_bin).unwrap();
        write_config(&config, &workspace);
        write_fake_docker(
            &fake_bin.join("docker"),
            &root.join("docker.calls"),
            &workspace,
            &root,
        );
        let proposal = invoke(
            &root,
            &fake_bin,
            &state,
            &[
                "local",
                "propose",
                "--config-root",
                config.to_str().unwrap(),
                "--environment",
                "test",
                "--image-family",
                "job-code",
                "--network-class",
                "none",
                "--tool-approval",
                "auto",
                "--workspace",
                &format!("primary={}:rw", workspace.display()),
                "--primary-workspace",
                "primary",
                "--timeout-secs",
                "30",
                "--prompt-stdin",
                "--output",
                authorization.to_str().unwrap(),
            ],
            Some("Return success"),
        );
        assert_success(&proposal);
        let proposal_stdout = String::from_utf8(proposal.stdout).unwrap();
        let digest = proposal_stdout
            .lines()
            .find_map(|line| line.strip_prefix("Authorization digest: "))
            .unwrap();
        let authorize = invoke(
            &root,
            &fake_bin,
            &state,
            &[
                "local",
                "authorize",
                "--environment",
                "test",
                "--image-family",
                "job-code",
                "--network-class",
                "none",
                "--tool-approval",
                "auto",
                "--timeout-secs",
                "30",
                "--authorization",
                authorization.to_str().unwrap(),
                "--digest",
                digest,
            ],
            None,
        );
        assert_success(&authorize);
        let run_args = [
            "local",
            "run",
            "--environment",
            "test",
            "--image-family",
            "job-code",
            "--network-class",
            "none",
            "--tool-approval",
            "auto",
            "--timeout-secs",
            "30",
            "--authorization",
            authorization.to_str().unwrap(),
            "--digest",
            digest,
        ];
        let interrupted = invoke_with_env(
            &root,
            &fake_bin,
            &state,
            &run_args,
            None,
            Some(("AGENT_HUB_TEST_ATTENDED_FAULT", boundary)),
        );
        assert!(
            !interrupted.status.success(),
            "fault {boundary} did not interrupt execution"
        );
        assert!(String::from_utf8_lossy(&interrupted.stderr).contains(boundary));

        let run_id = only_json_stem(&state.join("executions"));
        let restarted = invoke(&root, &fake_bin, &state, &run_args, None);
        assert!(!restarted.status.success());
        let status = invoke(
            &root,
            &fake_bin,
            &state,
            &["local", "status", "--run-id", &run_id],
            None,
        );
        let record: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(
            record["launch_state"],
            "terminal",
            "boundary {boundary}; restart stderr={}; record={record:#}",
            String::from_utf8_lossy(&restarted.stderr)
        );
        let authorization_record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                state
                    .join("authorizations/records")
                    .join(format!("{digest}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(authorization_record["active_attempt_id"].is_null());
        assert_eq!(authorization_record["attempts"][0]["state"], "terminal");
        if boundary == "terminal_record_published" {
            assert_success(&status);
            assert_eq!(record["status"], "succeeded");
            assert!(String::from_utf8_lossy(&restarted.stderr).contains("already consumed"));
            let published: serde_json::Value = serde_json::from_slice(
                &std::fs::read(state.join("runs").join(&run_id).join("result.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(published["launch_state"], "terminal");
            assert_eq!(published["attempt_id"], record["attempt_id"]);
        } else {
            assert_eq!(record["status"], "failed");
            assert!(
                record["error"]
                    .as_str()
                    .unwrap()
                    .contains("authorization consumed fail-closed")
            );
            assert!(String::from_utf8_lossy(&restarted.stderr).contains("consumed fail-closed"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(any())]
fn only_json_stem(path: &Path) -> String {
    let entries = std::fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    entries[0]
        .path()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !path.is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn initialize_repository(path: &Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(path.join("tracked.txt"), "tracked\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&[
        "-c",
        "user.name=Agent Hub Test",
        "-c",
        "user.email=agent-hub@example.com",
        "commit",
        "-q",
        "-m",
        "initial",
    ]);
}

#[test]
fn release_image_producer_qualifies_before_publish_and_cleans_only_candidate_reference() {
    let fixture = build_image_fixture("success");
    let output = invoke_image_build(&fixture);
    assert_success(&output);
    let events = std::fs::read_to_string(fixture.join("docker.events")).unwrap();
    let preflight = events.find("run ").unwrap();
    let publish = events.find("image tag ").unwrap();
    assert!(
        preflight < publish,
        "qualification did not precede publication: {events}"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("docker-state.json")).unwrap()).unwrap();
    assert_eq!(state["release"], format!("sha256:{}", "a".repeat(64)));
    assert_eq!(state["shared"], format!("sha256:{}", "a".repeat(64)));
    assert!(state["candidate"].is_null());
    assert!(!events.contains(&format!("image rm sha256:{}", "a".repeat(64))));
    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn release_image_producer_preserves_old_tag_on_qualification_failure() {
    let fixture = build_image_fixture("qualification-fail");
    let output = invoke_image_build(&fixture);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("injected qualification failure"));
    let events = std::fs::read_to_string(fixture.join("docker.events")).unwrap();
    assert!(!events.contains("image tag "));
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.join("docker-state.json")).unwrap()).unwrap();
    assert_eq!(state["release"], format!("sha256:{}", "b".repeat(64)));
    assert_eq!(state["shared"], format!("sha256:{}", "a".repeat(64)));
    assert!(state["candidate"].is_null());
    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn release_image_producer_refuses_occupied_candidate_without_removing_it() {
    let fixture = build_image_fixture("occupied");
    let output = invoke_image_build(&fixture);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("candidate image reference already exists")
    );
    let events = std::fs::read_to_string(fixture.join("docker.events")).unwrap();
    assert!(!events.contains("build "));
    assert!(!events.contains("image rm "));
    assert!(!events.contains("run "));
    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn release_image_producer_treats_explicit_missing_candidate_as_clean() {
    let fixture = build_image_fixture("build-fail");
    let output = invoke_image_build(&fixture);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("docker build failed"));
    assert!(!stderr.contains("failed to remove owned candidate reference"));
    let events = std::fs::read_to_string(fixture.join("docker.events")).unwrap();
    assert!(events.contains("image rm agent-hub/job-code-build-candidate:"));
    std::fs::remove_dir_all(fixture).unwrap();
}

fn build_image_fixture(mode: &str) -> PathBuf {
    let root = temp_root();
    let fake_bin = root.join("fake-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("images/job-code")).unwrap();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    let commit = git(&["rev-parse", "HEAD"]);
    let archive = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["archive", "--format=tar", &commit])
        .output()
        .unwrap();
    assert!(archive.status.success());
    let archive_sha256 = config::sha256_hex(&archive.stdout);
    std::fs::write(
        root.join("system.toml"),
        "[system]\noperator='test'\nbot_identity='test'\n[system.images]\ninstalled=['job-code']\n",
    )
    .unwrap();
    std::fs::write(
        root.join("images/job-code/image.toml"),
        format!(
            "[image]\nname='job-code'\ntag='{IMAGE_TAG}'\ndescription='test'\n[image.build_contexts.agent-hub-source]\ntype='git'\nurl='https://github.com/nickdavies/agent-hub.git'\ncommit='{commit}'\narchive_sha256='{archive_sha256}'\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("images/job-code/Dockerfile"),
        LOCKED_BUILD_DOCKERFILE,
    )
    .unwrap();
    let report = preflight_report(unsafe { libc::geteuid() }, unsafe { libc::getegid() });
    std::fs::write(
        root.join("preflight.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("docker-state.json"),
        serde_json::to_vec(&serde_json::json!({
            "release": format!("sha256:{}", "b".repeat(64)),
            "shared": format!("sha256:{}", "a".repeat(64)),
            "candidate": null,
        }))
        .unwrap(),
    )
    .unwrap();
    let docker = fake_bin.join("docker");
    let script = FAKE_BUILD_DOCKER
        .replace("@ROOT@", &root.display().to_string())
        .replace("@MODE@", mode);
    std::fs::write(&docker, script).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn invoke_image_build(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-hub"))
        .args(["build", "--config-root"])
        .arg(root)
        .arg("images")
        .env_clear()
        .env("HOME", root.join("home"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("fake-bin").display()),
        )
        .output()
        .unwrap()
}

const FAKE_BUILD_DOCKER: &str = r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path('@ROOT@')
mode = '@MODE@'
args = sys.argv[1:]
events = root / 'docker.events'
with events.open('a') as handle:
    handle.write(' '.join(args) + '\n')
state_path = root / 'docker-state.json'
state = json.loads(state_path.read_text())
new = 'sha256:' + 'a' * 64
if args[:2] == ['image', 'inspect'] and not any(value.startswith('--format=') for value in args):
    reference = args[2]
    if mode == 'occupied':
        print(new)
        raise SystemExit(0)
    print(f'Error response from daemon: No such image: {reference}', file=sys.stderr)
    raise SystemExit(1)
if args and args[0] == 'build':
    if mode == 'build-fail':
        print('injected build failure', file=sys.stderr)
        raise SystemExit(75)
    iidfile = pathlib.Path(args[args.index('--iidfile') + 1])
    candidate = args[args.index('-t') + 1]
    iidfile.write_text(new + '\n')
    state['candidate'] = candidate
    state_path.write_text(json.dumps(state))
    raise SystemExit(0)
if args[:2] == ['image', 'inspect']:
    print(new)
    raise SystemExit(0)
if args and args[0] == 'run':
    if state['release'] != 'sha256:' + 'b' * 64:
        print('release tag moved before qualification', file=sys.stderr)
        raise SystemExit(76)
    if mode == 'qualification-fail':
        print('injected qualification failure', file=sys.stderr)
        raise SystemExit(75)
    print((root / 'preflight.json').read_text(), end='')
    raise SystemExit(0)
if args[:2] == ['image', 'tag']:
    state['release'] = args[2]
    state_path.write_text(json.dumps(state))
    raise SystemExit(0)
if args[:2] == ['image', 'rm']:
    reference = args[2]
    if state['candidate'] == reference:
        state['candidate'] = None
        state_path.write_text(json.dumps(state))
        raise SystemExit(0)
    print(f'Error response from daemon: No such image: {reference}', file=sys.stderr)
    raise SystemExit(1)
print('unexpected docker invocation: ' + ' '.join(args), file=sys.stderr)
raise SystemExit(2)
"#;

const LOCKED_BUILD_DOCKERFILE: &str = r#"FROM debian:bookworm-slim@sha256:abc
ENV DEBIAN_FRONTEND=noninteractive
ENV DEBIAN_SNAPSHOT=20260803T000000Z
RUN printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/%s bookworm-security main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false -o Acquire::Check-Valid-Until=false update \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s bookworm main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/%s bookworm-security main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list
RUN apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false -o Acquire::Check-Valid-Until=false update \
    && apt-get -o Acquire::AllowInsecureRepositories=false -o Acquire::AllowDowngradeToInsecureRepositories=false -o APT::Get::AllowUnauthenticated=false install -y --no-install-recommends curl git \
    && rm -rf /var/lib/apt/lists/*
ENV TOOL_VERSION=1.0
ENV TOOL_SHA256=abc
RUN curl -fsSL "https://example.com/tool-${TOOL_VERSION}" -o /tmp/tool \
    && printf '%s  %s\n' "$TOOL_SHA256" /tmp/tool | sha256sum -c -
COPY --from=agent-hub-source / /opt/src/agent-hub
RUN cd /opt/src/agent-hub && cargo build --locked --release -p agent-hub-gateway
"#;

fn invoke(
    root: &Path,
    fake_bin: &Path,
    state: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    invoke_with_env(root, fake_bin, state, args, stdin, None)
}

fn invoke_with_env(
    root: &Path,
    fake_bin: &Path,
    state: &Path,
    args: &[&str],
    stdin: Option<&str>,
    environment: Option<(&str, &str)>,
) -> Output {
    use std::io::Write as _;
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-hub"));
    command
        .args(args)
        .env_clear()
        .current_dir(root)
        .env("AGENT_HUB_ATTENDED_STATE", state)
        .env("HOST_ENV_MUST_NOT_LEAK", "sentinel")
        .env("HOME", root.join("home"))
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some((name, value)) = environment {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap();
    if let Some(stdin) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_config(root: &Path, workspace: &Path) {
    std::fs::create_dir_all(root.join("network_policies/none")).unwrap();
    std::fs::create_dir_all(root.join("environments/test")).unwrap();
    std::fs::create_dir_all(root.join("images/job-code")).unwrap();
    std::fs::write(
        root.join("system.toml"),
        format!(
            "[system]\noperator='test'\nbot_identity='test'\n[system.repo_discovery]\nenabled=true\n[[system.repo_discovery.strategies]]\ntype='directory'\npath='{}'\ninclude=[]\nexclude=[]\n[system.environments]\ninstalled=['test']\ntriage_environment='test'\n[system.network_policies]\ninstalled=['none']\n[system.images]\ninstalled=['job-code']\n",
            workspace.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("local-agent.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_agent":{"kind":"generic","command":"true"},
            "triage_repos":[workspace],
            "root_registry":{"roots":[{"identity":"workspace","path":workspace.parent().unwrap(),"roles":["workspace_ro","workspace_rw","context_ro","git_metadata"]}]},
            "triage_mounts":[{"host_path":workspace,"container_path":"/repos/workspace","read_only":true}],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("network_policies/none/policy.toml"),
        "[policy]\nname='none'\ndescription='none'\ndocker_network='none'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("environments/test/environment.toml"),
        format!("[environment]\nname='test'\ndescription='test'\nimage='{IMAGE_TAG}'\nnetwork_policy='none'\napproval_mode='auto'\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("environments/test/agent-hub-permissions.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(root.join("environments/test/dippy.conf"), "allow true\n").unwrap();
    std::fs::write(
        root.join("environments/test/opencode.json"),
        r#"{"model":"openai/gpt-5.6-sol","small_model":"openai/gpt-5.6-sol","enabled_providers":["openai"],"permission":{"*":"allow"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("images/job-code/image.toml"),
        format!("[image]\nname='job-code'\ntag='{IMAGE_TAG}'\ndescription='test'\n"),
    )
    .unwrap();
}

fn write_fake_docker(path: &Path, marker: &Path, workspace: &Path, root: &Path) {
    use std::os::unix::fs::MetadataExt as _;
    let report = preflight_report(
        std::fs::metadata(workspace).unwrap().uid(),
        std::fs::metadata(workspace).unwrap().gid(),
    );
    let report_path = root.join("preflight.json");
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    let result_event = serde_json::json!({
        "type": "text",
        "part": {
            "text": serde_json::json!({
                "schema": "agent_result_v1",
                "complete": true,
                "summary": "executed",
                "acceptance_results": [],
                "blockers": [],
                "artifact_references": []
            }).to_string()
        }
    })
    .to_string();
    std::fs::write(
        path,
        format!(
             "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in 'container inspect'|'network inspect') printf '%s %s\\n' \"$(date +%s%N)\" \"$*\" >> '{}';; esac\nif [ \"$1 $2\" = 'image inspect' ]; then if [ -f '{}' ]; then rm '{}'; printf 'injected image inspection failure\\n' >&2; exit 75; fi; printf '%s\\n' 'sha256:{}'; exit 0; fi\nif [ \"$1 $2\" = 'container inspect' ]; then printf '%s\\n' 'No such container' >&2; exit 1; fi\nif [ \"$1 $2\" = 'network inspect' ]; then printf '%s\\n' 'No such network' >&2; exit 1; fi\ncase \"$*\" in *agent-hub-job-code-preflight*) if [ -f '{}' ]; then rm '{}'; printf 'injected preflight failure\\n' >&2; exit 75; fi; cat '{}' ;; *) if [ -f '{}' ]; then rm '{}'; printf '%s\\n' '{}'; printf 'Error: Service Unavailable\\n' >&2; exit 75; fi; if [ -f '{}' ]; then printf 'entered\\n' > '{}'; sync '{}'; sleep 10; fi; printf '%s\\n' '{}' ;; esac\n",
             marker.display(),
             root.join("cleanup.events").display(),
             root.join("fail-image-inspect-once").display(),
             root.join("fail-image-inspect-once").display(),
             "a".repeat(64),
            root.join("fail-preflight-once").display(),
            root.join("fail-preflight-once").display(),
             report_path.display(),
            root.join("provider-503-once").display(),
            root.join("provider-503-once").display(),
            serde_json::json!({
                "type": "error",
                "timestamp": 1788220800000_u64,
                "sessionID": "session-1",
                "error": {
                    "name": "APIError",
                    "data": {
                        "message": "Error: Service Unavailable",
                        "statusCode": 503,
                        "isRetryable": true
                    }
                }
            }),
             root.join("slow-launch").display(),
            root.join("docker-entered").display(),
            root.join("docker-entered").display(),
            result_event,
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_fake_run_gateway(path: &Path, decision: &Path, state: &Path) {
    std::fs::write(
        path,
        format!(
            concat!(
                "#!/bin/sh\n[ \"$HOME\" = '{}' ] || {{ echo 'wrong gateway HOME' >&2; exit 2; }}\n[ -z \"${{HOST_ENV_MUST_NOT_LEAK+x}}\" ] && [ -z \"${{AGENT_HUB_ATTENDED_STATE+x}}\" ] || {{ echo 'host environment leaked' >&2; exit 2; }}\n",
                "case \"$PATH\" in *'{}'*) echo 'host PATH leaked' >&2; exit 2;; esac\n",
                "for arg in \"$@\"; do if [ \"$arg\" = --preview ]; then exec /usr/bin/python3 -c 'import json,sys; r=json.load(sys.stdin)[\"tool_input\"]; print(json.dumps({{\"schema\":\"agent_hub_run_policy_preview_v1\",\"projection\":r[\"policy_projection\"],\"matched_action\":\"allow\"}}))'; fi; done\n",
                "[ ! -e '{}/authorizations' ] && [ ! -e '{}/runs' ] || true\ncase \"$(cat '{}')\" in pending*) echo $$ > '{}'; printf entered > '{}'; trap 'printf confirmed > \"{}\"; exit 1' TERM INT; while :; do sleep 1; done;; esac\nexec /usr/bin/python3 -c 'import datetime,json,sys; h=json.load(sys.stdin); r=h[\"tool_input\"]; p=open(\"{}\").read().split(); d,a=p[:2]; mode=p[2] if len(p)>2 else \"\"; digest=\"0\"*64 if mode==\"wrong\" else r[\"canonical_digest\"]; expiry=\"2000-01-01T00:00:00Z\" if mode==\"expired\" else r[\"expires_at\"]; approval=\"018f0f21-76c0-7b37-a446-31f82a5e7601\" if a==\"human\" else r[\"invocation_id\"]; print(json.dumps({{\"schema\":\"agent_hub_run_decision_v1\",\"invocation_id\":r[\"invocation_id\"],\"canonical_digest\":digest,\"request_digest\":r[\"request_digest\"],\"decision\":d,\"authority\":a,\"approval_id\":approval,\"reason\":None,\"decided_at\":datetime.datetime.now(datetime.timezone.utc).isoformat().replace(\"+00:00\",\"Z\"),\"expires_at\":expiry}}))'\n",
            ),
            state.parent().unwrap().join("home").display(),
            path.parent().unwrap().display(),
            state.display(),
            state.display(),
            decision.display(),
            state.parent().unwrap().join("gateway.pid").display(),
            state.parent().unwrap().join("gateway-entered").display(),
            state
                .parent()
                .unwrap()
                .join("gateway-cancel-confirmed")
                .display(),
            decision.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn preflight_report(uid: u32, gid: u32) -> serde_json::Value {
    let tools = [
        "agent_hub_gateway",
        "cargo",
        "cc",
        "cmake",
        "corepack",
        "curl",
        "cxx",
        "git",
        "gh",
        "go",
        "jq",
        "make",
        "ninja",
        "node",
        "npm",
        "opencode",
        "pip",
        "pkg_config",
        "pnpm",
        "python",
        "rg",
        "rustc",
        "unbound",
        "unbound_checkconf",
        "uv",
        "yarn",
    ]
    .into_iter()
    .map(|name| {
        let version = if name == "opencode" {
            "1.18.18".to_string()
        } else {
            format!("{name} 1.0")
        };
        (name.to_string(), serde_json::json!(version))
    })
    .collect::<serde_json::Map<_, _>>();
    let sources = [
        "agent_hub",
        "gh",
        "go",
        "node",
        "opencode",
        "pnpm",
        "rust_cargo",
        "rust_channel_manifest",
        "rust_std",
        "rustc",
        "uv",
        "yarn",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_string(),
            serde_json::json!({
                "url":format!("https://example.com/{name}-1.0.tar.xz"),
                "digest":{"algorithm":"sha256","value":"b".repeat(64)}
            }),
        )
    })
    .collect::<serde_json::Map<_, _>>();
    let manifest = serde_json::json!({
        "schema":"agent-hub.job-code-tools.v3",
        "image":IMAGE_TAG,
        "debian_snapshot":"20260824T000000Z",
        "runtime":{"rust_home":"/opt/rust","rust_bin":"/opt/rust/bin","cargo_home":"$HOME/.cargo"},
        "tools":tools,
        "provenance":{"packages":{"unbound":{"version":"1.17.1-2+deb12u4","snapshot":"20260824T000000Z","uid":100,"gid":101}},"sources":sources},
    });
    let contents = serde_json::to_string(&manifest).unwrap();
    serde_json::json!({
        "schema":"agent-hub.job-code-preflight.v3",
        "status":"ok",
        "image":{"resolved":format!("sha256:{}", "a".repeat(64))},
        "requested":{"uid":uid,"gid":gid},
        "runtime":{"uid":uid,"gid":gid},
        "manifest":{"path":config::JOB_CODE_TOOL_MANIFEST_PATH,"sha256":config::job_code_tool_manifest_sha256(&contents),"contents":contents},
    })
}

fn temp_root() -> PathBuf {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/test-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let root = parent.join(format!("authorization-process-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    root
}

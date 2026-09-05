use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IMAGE_TAG: &str = "agent-hub/job-code:2026-09-04";

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
    let config = root.join("config");
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
    write_fake_docker(&fake_bin.join("docker"), &docker_marker, &workspace, &root);
    write_fake_run_gateway(&gateway_script, &decision, &state, &docker_marker);
    let host_dir = root.join("home/.config/agent-hub");
    std::fs::create_dir_all(&host_dir).unwrap();
    let gateway_config = host_dir.join("permissions.json");
    std::fs::write(&gateway_config, "{}").unwrap();
    let plugin_install = host_dir.join("opencode-plugin.js");
    std::fs::write(&plugin_install, "export default {}\n").unwrap();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_agent-hub"));
    let config_root = std::fs::canonicalize(&config).unwrap();
    let config_metadata = std::fs::metadata(&config_root).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    let pin = |path: &Path, source: &str| {
        let metadata = std::fs::metadata(path).unwrap();
        let sha256 = protocol::sha256_hex(&std::fs::read(path).unwrap());
        serde_json::json!({
            "path":std::fs::canonicalize(path).unwrap(),
            "sha256":sha256,
            "source_identity":format!("{source}:sha256:{sha256}"),
            "device":metadata.dev(),
            "inode":metadata.ino(),
        })
    };
    let config_artifacts = [
        "system.toml",
        "local-agent.json",
        "network_policies/none/policy.toml",
        "environments/test/environment.toml",
        "environments/test/agent-hub-permissions.json",
        "environments/test/dippy.conf",
        "environments/test/opencode.json",
        "images/job-code/image.toml",
    ]
    .into_iter()
    .map(|relative| pin(&config.join(relative), &format!("config:{relative}")))
    .collect::<Vec<_>>();
    std::fs::write(
        host_dir.join("host-run.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema":"agent_hub_run_host_v1",
            "config_root":config_root,
            "config_root_identity":{
                "canonical_path":config_root,
                "device":config_metadata.dev(),
                "inode":config_metadata.ino()
            },
            "config_artifacts":config_artifacts,
            "self_binary":pin(&executable, "runtime:agent-hub"),
            "gateway":pin(&gateway_script, "runtime:agent-hub-gateway"),
            "policy":pin(&gateway_config, "policy:agent-hub-run"),
            "opencode_plugin_install":pin(&plugin_install, "plugin:opencode-1.18.18"),
            "image_digests":{"test":format!("sha256:{}", "a".repeat(64))}
        }))
        .unwrap(),
    )
    .unwrap();
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

    let host_config_path = host_dir.join("host-run.json");
    let baseline_host_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&host_config_path).unwrap()).unwrap();
    for (invocation, mutate) in [
        ("018f0f21-76c0-7b37-a446-31f82a5e7610", "missing"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7611", "extra"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7612", "source"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7614", "same-role-missing"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7615", "path"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7616", "alias"),
        ("018f0f21-76c0-7b37-a446-31f82a5e7617", "duplicate-relative"),
        (
            "018f0f21-76c0-7b37-a446-31f82a5e7618",
            "second-same-role-missing",
        ),
        ("018f0f21-76c0-7b37-a446-31f82a5e7620", "digest"),
    ] {
        let mut changed = baseline_host_config.clone();
        match mutate {
            "missing" => {
                changed["config_artifacts"].as_array_mut().unwrap().pop();
            }
            "extra" => {
                let pin = &mut changed["config_artifacts"].as_array_mut().unwrap()[3];
                let digest = pin["sha256"].as_str().unwrap();
                pin["source_identity"] = format!("config:unexpected:sha256:{digest}").into();
            }
            "source" => {
                changed["gateway"]["source_identity"] = "unbound-source".into();
            }
            "same-role-missing" => {
                changed["config_artifacts"]
                    .as_array_mut()
                    .unwrap()
                    .remove(4);
            }
            "path" => {
                let pins = changed["config_artifacts"].as_array_mut().unwrap();
                pins[3]["path"] = pins[4]["path"].clone();
            }
            "alias" => {
                let pin = &mut changed["config_artifacts"].as_array_mut().unwrap()[3];
                let path = pin["path"].as_str().unwrap();
                pin["path"] = path.replace("/environments/", "/environments/./").into();
            }
            "duplicate-relative" => {
                let pins = changed["config_artifacts"].as_array_mut().unwrap();
                let digest = pins[4]["sha256"].as_str().unwrap();
                pins[4]["source_identity"] =
                    format!("config:environments/test/environment.toml:sha256:{digest}").into();
            }
            "second-same-role-missing" => {
                changed["config_artifacts"]
                    .as_array_mut()
                    .unwrap()
                    .remove(5);
            }
            "digest" => {
                let pin = &mut changed["config_artifacts"].as_array_mut().unwrap()[3];
                let digest = "0".repeat(64);
                pin["sha256"] = digest.clone().into();
                pin["source_identity"] =
                    format!("config:environments/test/environment.toml:sha256:{digest}").into();
            }
            _ => unreachable!(),
        }
        std::fs::write(&host_config_path, serde_json::to_vec(&changed).unwrap()).unwrap();
        let rejected = invoke(
            &root,
            &fake_bin,
            &state,
            &[
                "local",
                "run",
                "--request-stdin",
                "--internal-invocation-id",
                invocation,
            ],
            Some(&launch.to_string()),
        );
        assert!(!rejected.status.success());
        let output: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(output["approval_receipt"]["status"], "unavailable");
        assert!(
            !state
                .join(format!("authorizations/records/{invocation}.json"))
                .exists()
        );
    }
    std::fs::write(
        &host_config_path,
        serde_json::to_vec(&baseline_host_config).unwrap(),
    )
    .unwrap();

    let unexpected_config = config.join("environments/test/unexpected.conf");
    std::fs::write(&unexpected_config, "unexpected\n").unwrap();
    let rejected = invoke(
        &root,
        &fake_bin,
        &state,
        &[
            "local",
            "run",
            "--request-stdin",
            "--internal-invocation-id",
            "018f0f21-76c0-7b37-a446-31f82a5e7619",
        ],
        Some(&launch.to_string()),
    );
    assert!(!rejected.status.success());
    let output: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert!(
        output["error"]
            .as_str()
            .unwrap()
            .contains("pinned config artifact coverage mismatch")
    );
    std::fs::remove_file(unexpected_config).unwrap();

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
    assert!(!docker_marker.exists());
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
        assert!(!docker_marker.exists());
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
        !docker_marker.exists(),
        "denied one-step launch invoked Docker"
    );
    let denied_json: serde_json::Value = serde_json::from_slice(&denied.stdout).unwrap();
    assert_eq!(
        denied_json["approval_receipt"]["receipt"]["decision"],
        "deny"
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
    std::fs::write(root.join("fail-preflight-once"), "fail").unwrap();
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
        record["receipt_id"],
        output["approval_receipt"]["receipt"]["approval_id"]
    );
    assert_eq!(
        record["decision_receipt"],
        output["approval_receipt"]["receipt"]
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

    std::fs::write(&plugin_install, "tampered\n").unwrap();
    let tampered = invoke(
        &root,
        &fake_bin,
        &state,
        &allowed_args,
        Some(&launch.to_string()),
    );
    assert!(!tampered.status.success());
    let tampered: serde_json::Value = serde_json::from_slice(&tampered.stdout).unwrap();
    assert!(
        tampered["error"]
            .as_str()
            .unwrap()
            .contains("pinned artifact hash mismatch")
    );
    std::fs::write(&plugin_install, "export default {}\n").unwrap();

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
            .contains("without following links")
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
        .current_dir(root)
        .env("AGENT_HUB_ATTENDED_STATE", state)
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
        format!("[image]\nname='job-code'\ntag='{IMAGE_TAG}'\n"),
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
             "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1 $2\" = 'image inspect' ]; then printf '%s\\n' 'sha256:{}'; exit 0; fi\nif [ \"$1 $2\" = 'container inspect' ]; then printf '%s\\n' 'No such container' >&2; exit 1; fi\nif [ \"$1 $2\" = 'network inspect' ]; then printf '%s\\n' 'No such network' >&2; exit 1; fi\ncase \"$*\" in *agent-hub-job-code-preflight*) if [ -f '{}' ]; then rm '{}'; printf 'injected preflight failure\\n' >&2; exit 75; fi; cat '{}' ;; *) if [ -f '{}' ]; then rm '{}'; printf '%s\\n' '{}'; printf 'Error: Service Unavailable\\n' >&2; exit 75; fi; if [ -f '{}' ]; then printf 'entered\\n' > '{}'; sync '{}'; sleep 10; fi; printf '%s\\n' '{}' ;; esac\n",
            marker.display(),
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

fn write_fake_run_gateway(path: &Path, decision: &Path, state: &Path, docker_marker: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n[ ! -e '{}/authorizations' ] && [ ! -e '{}/runs' ] || true\ncase \"$(cat '{}')\" in deny*) [ ! -e '{}' ] || {{ echo 'docker ran before deny decision' >&2; exit 2; }};; pending*) echo $$ > '{}'; printf entered > '{}';; esac\nexec /usr/bin/python3 -c 'import datetime,json,sys,time; h=json.load(sys.stdin); r=h[\"tool_input\"]; p=open(\"{}\").read().split(); d,a=p[:2]; mode=p[2] if len(p)>2 else \"\"; time.sleep(60) if d==\"pending\" else None; digest=\"0\"*64 if mode==\"wrong\" else r[\"canonical_digest\"]; expiry=\"2000-01-01T00:00:00Z\" if mode==\"expired\" else r[\"expires_at\"]; approval=\"018f0f21-76c0-7b37-a446-31f82a5e7601\" if a==\"human\" else r[\"invocation_id\"]; print(json.dumps({{\"schema\":\"agent_hub_run_decision_v1\",\"invocation_id\":r[\"invocation_id\"],\"canonical_digest\":digest,\"request_digest\":r[\"request_digest\"],\"decision\":d,\"authority\":a,\"approval_id\":approval,\"reason\":None,\"decided_at\":datetime.datetime.now(datetime.timezone.utc).isoformat().replace(\"+00:00\",\"Z\"),\"expires_at\":expiry}}))'\n",
            state.display(),
            state.display(),
            decision.display(),
            docker_marker.display(),
            state.parent().unwrap().join("gateway.pid").display(),
            state.parent().unwrap().join("gateway-entered").display(),
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
    .map(|name| (name.to_string(), serde_json::json!(format!("{name} 1.0"))))
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

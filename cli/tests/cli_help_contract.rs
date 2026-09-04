use std::process::Command;

fn agent_hub(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agent-hub"))
        .args(args)
        .output()
        .expect("execute agent-hub")
}

fn agent_hub_with_state(state: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agent-hub"))
        .args(args)
        .env("AGENT_HUB_ATTENDED_STATE", state)
        .output()
        .expect("execute agent-hub with attended state")
}

#[test]
fn executable_exposes_only_the_local_namespace() {
    let help = agent_hub(&["--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.contains("local"));
    assert!(!stdout.contains("attended"));

    let alias = agent_hub(&["attended", "--help"]);
    assert!(!alias.status.success());
}

#[test]
fn executable_local_help_has_the_complete_command_contract() {
    let help = agent_hub(&["local", "--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    for command in [
        "run",
        "inspect",
        "list",
        "status",
        "logs",
        "result",
        "artifacts",
        "repositories",
        "cleanup-result",
        "cancel",
        "cleanup",
    ] {
        assert!(stdout.contains(command), "missing command {command}");
    }
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("propose "))
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("authorize "))
    );

    for command in [
        "status",
        "logs",
        "result",
        "artifacts",
        "repositories",
        "cleanup-result",
        "cancel",
        "cleanup",
    ] {
        let help = agent_hub(&["local", command, "--help"]);
        assert!(help.status.success());
        assert!(String::from_utf8(help.stdout).unwrap().contains("--run-id"));
    }

    let command_snapshot = stdout
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("run ")
                || line.trim_start().starts_with("inspect ")
                || line.trim_start().starts_with("list ")
                || line.trim_start().starts_with("status ")
                || line.trim_start().starts_with("logs ")
                || line.trim_start().starts_with("result ")
                || line.trim_start().starts_with("artifacts ")
                || line.trim_start().starts_with("repositories ")
                || line.trim_start().starts_with("cleanup-result ")
                || line.trim_start().starts_with("cancel ")
                || line.trim_start().starts_with("cleanup ")
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        command_snapshot,
        "run             Host-owned one-step prepare, approve, and execute path (structured JSON on stdin)\ninspect         Debug canonical preparation without requesting permission or executing\nlist            List local attended executions\nstatus          Show one execution record\nlogs            Print an execution log\nresult          Print an execution result\nartifacts       Print the bounded artifact manifest\nrepositories    Print the bounded repository result document\ncleanup-result  Print the bounded cleanup result document\ncancel          Cancel an owned running execution\ncleanup         Explicitly remove owned execution resources and artifacts"
    );
}

#[test]
fn result_document_commands_snapshot_bounds_and_failures() {
    for command in ["result", "artifacts", "repositories", "cleanup-result"] {
        let help = agent_hub(&["local", command, "--help"]);
        assert!(help.status.success());
        let stdout = String::from_utf8(help.stdout).unwrap();
        let normalized = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("--run-id <RUN_ID>"),
            "{command}: {normalized}"
        );
        assert!(
            normalized.contains("--limit-bytes <LIMIT_BYTES>"),
            "{command}: {normalized}"
        );

        for limit in ["0", "16777217"] {
            let output = agent_hub(&[
                "local",
                command,
                "--run-id",
                "00000000-0000-0000-0000-000000000000",
                "--limit-bytes",
                limit,
            ]);
            assert!(!output.status.success(), "{command} accepted {limit}");
        }

        for limit in ["1", "16777216"] {
            let output = agent_hub(&[
                "local",
                command,
                "--run-id",
                "00000000-0000-0000-0000-000000000000",
                "--limit-bytes",
                limit,
            ]);
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                !stderr.contains("invalid value"),
                "{command} rejected boundary {limit}: {stderr}"
            );
        }
    }
}

#[test]
fn result_commands_print_failure_evidence_before_returning_nonzero() {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-cli-result-failure-{}",
        uuid::Uuid::new_v4()
    ));
    let executions = root.join("executions");
    std::fs::create_dir_all(&executions).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&executions, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let record: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/attended-result-v3-legacy.json")).unwrap();
    let id = record["id"].as_str().unwrap();
    let record_path = executions.join(format!("{id}.json"));
    std::fs::write(&record_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&record_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let status = agent_hub_with_state(&root, &["local", "status", "--run-id", id]);
    assert!(!status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["schema"], "attended_result_v3");
    assert!(String::from_utf8_lossy(&status.stderr).contains("result outcome Unknown"));

    let result = agent_hub_with_state(&root, &["local", "result", "--run-id", id]);
    assert!(!result.status.success());
    let result_json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result_json["schema"], "local_result_v2");
    assert_eq!(result_json["result_outcome"], "unknown");
    for name in ["result", "artifacts", "repositories", "cleanup"] {
        assert_eq!(result_json[name]["reference"]["available"], false);
    }

    for command in ["artifacts", "repositories", "cleanup-result"] {
        let output = agent_hub_with_state(&root, &["local", command, "--run-id", id]);
        assert!(!output.status.success(), "{command} unexpectedly succeeded");
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["reference"]["available"], false);
        assert_eq!(document["parse_error"], "document is unavailable");
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executable_run_help_exposes_one_step_structured_input_without_bypasses() {
    let help = agent_hub(&["local", "run", "--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    for flag in ["--request-stdin", "--approvals-server"] {
        assert!(stdout.contains(flag), "missing one-step run flag {flag}");
    }
    for forbidden in [
        "--authorization",
        "--digest",
        "--allow",
        "--bypass",
        "--skip-approval",
        "--workspace-export-root",
    ] {
        assert!(
            !stdout.contains(forbidden),
            "unexpected bypass/legacy flag {forbidden}"
        );
    }
}

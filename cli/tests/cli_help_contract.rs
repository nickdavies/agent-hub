use std::process::Command;

fn agent_hub(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agent-hub"))
        .args(args)
        .output()
        .expect("execute agent-hub")
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
        "run", "inspect", "list", "status", "logs", "result", "cancel", "cleanup",
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

    for command in ["status", "logs", "result", "cancel", "cleanup"] {
        let help = agent_hub(&["local", command, "--help"]);
        assert!(help.status.success());
        assert!(String::from_utf8(help.stdout).unwrap().contains("--run-id"));
    }
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
    ] {
        assert!(
            !stdout.contains(forbidden),
            "unexpected bypass/legacy flag {forbidden}"
        );
    }
}

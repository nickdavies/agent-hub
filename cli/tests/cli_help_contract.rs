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
        "propose",
        "authorize",
        "run",
        "list",
        "status",
        "logs",
        "result",
        "cancel",
        "cleanup",
    ] {
        assert!(stdout.contains(command), "missing command {command}");
    }
    assert!(stdout.contains("agent-hub local propose --config-root"));
    assert!(stdout.contains("agent-hub local authorize --environment"));
    assert!(stdout.contains("agent-hub local run --environment"));
    let unwrapped = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(unwrapped.contains(
        "--network-class build --network-resource llm --tool-approval dippy --timeout-secs 3600 --authorization"
    ));
    assert!(
        stdout
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
fn executable_propose_help_has_only_stdin_prompt_input_and_exact_review_flags() {
    let help = agent_hub(&["local", "propose", "--help"]);
    assert!(help.status.success());
    let stdout = String::from_utf8(help.stdout).unwrap();
    for flag in [
        "--environment",
        "--image-family",
        "--network-class",
        "--network-resource",
        "--tool-approval",
        "--overlay",
        "--workspace",
        "--primary-workspace",
        "--context-root",
        "--timeout-secs",
        "--prompt-stdin",
    ] {
        assert!(stdout.contains(flag), "missing proposal flag {flag}");
    }
    assert!(!stdout.contains("--prompt-file"));
    assert!(!stdout.contains("--prompt <"));
}

use std::process::Command;

#[test]
fn production_binary_does_not_expose_file_adapter_test_helper() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-hub-server"))
        .arg("--help")
        .env("AGENT_HUB_FILE_HELPER", "1")
        .env("AGENT_HUB_FILE_HELPER_OP", "ensure")
        .output()
        .expect("run production server help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(!help.contains("file-helper"));
    assert!(!help.contains("AGENT_HUB_FILE_HELPER"));
    assert!(!help.contains("durable-cancellation-e2e"));
    assert!(!help.contains("AGENT_HUB_DURABLE_CANCELLATION_E2E"));
}

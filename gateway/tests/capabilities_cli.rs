use std::process::Command;

#[test]
fn capabilities_prints_the_strict_static_protocol_manifest() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-hub-gateway"))
        .arg("capabilities")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let capabilities: protocol::AgentHubCapabilitiesV1 =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capabilities, protocol::AgentHubCapabilitiesV1::current());
    capabilities.validate().unwrap();
    assert!(output.stdout.len() <= protocol::AGENT_HUB_CAPABILITIES_MAX_BYTES);
}

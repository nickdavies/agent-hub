use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use config::{
    ConfigAction, ConfigDecision, load_tool_config, resolve_action, validate_tool_config,
};
use protocol::Tool;
use serde_json::{Value, json};

static NEXT_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);

struct TestConfig {
    path: PathBuf,
}

impl TestConfig {
    fn new(rules: Value, default: &str) -> Self {
        let id = NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agent-hub-in-cwds-{}-{id}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version": 1,
                "default": default,
                "rules": rules,
            }))
            .expect("test config serializes"),
        )
        .expect("test config is writable");
        Self { path }
    }

    fn path(&self) -> &str {
        self.path.to_str().expect("temporary path is UTF-8")
    }
}

impl Drop for TestConfig {
    fn drop(&mut self) {
        fs::remove_file(&self.path).expect("test config is removable");
    }
}

fn resolve_tool(config: &config::ToolConfig, tool: Tool, cwd: Option<&str>) -> ConfigAction {
    resolve_action(config, &tool, &[], cwd, None)
}

fn resolve_mcp(config: &config::ToolConfig, cwd: Option<&str>) -> ConfigAction {
    resolve_tool(config, Tool::Unknown("mcp_weather_lookup".to_string()), cwd)
}

fn scoped_allow_config(roots: &[&str]) -> TestConfig {
    TestConfig::new(
        json!([{
            "tools": ["mcp_weather_lookup"],
            "action": "allow",
            "in_cwds": roots,
        }]),
        "deny",
    )
}

#[test]
fn in_cwds_matches_exact_mcp_cwd() {
    let file = scoped_allow_config(&["/home/user/project"]);
    let config = load_tool_config(file.path()).expect("config loads");

    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/project")),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn in_cwds_matches_nested_mcp_cwd() {
    let file = scoped_allow_config(&["/home/user/project"]);
    let config = load_tool_config(file.path()).expect("config loads");

    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/project/crates/gateway")),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn in_cwds_skips_rule_for_outside_mcp_cwd() {
    let file = scoped_allow_config(&["/home/user/project"]);
    let config = load_tool_config(file.path()).expect("config loads");

    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/other")),
        ConfigAction::Decision(ConfigDecision::Deny(_))
    ));
}

#[test]
fn in_cwds_rejects_directory_prefix_collision() {
    let file = scoped_allow_config(&["/home/user/project"]);
    let config = load_tool_config(file.path()).expect("config loads");

    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/project-other")),
        ConfigAction::Decision(ConfigDecision::Deny(_))
    ));
}

#[test]
fn in_cwds_matches_any_of_multiple_roots() {
    let file = scoped_allow_config(&["/home/user/project-a", "/home/user/project-b"]);
    let config = load_tool_config(file.path()).expect("config loads");

    for cwd in ["/home/user/project-a", "/home/user/project-b/nested"] {
        assert!(matches!(
            resolve_mcp(&config, Some(cwd)),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }
}

#[test]
fn in_cwds_normalizes_configured_root_lexically() {
    let file = scoped_allow_config(&["/home/user/project/./crates/../"]);
    let config = load_tool_config(file.path()).expect("config loads");

    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/project/src")),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn in_cwds_root_matches_every_absolute_cwd() {
    let file = scoped_allow_config(&["/"]);
    let config = load_tool_config(file.path()).expect("config loads");

    for cwd in ["/", "/home/user/project", "/var/empty"] {
        assert!(matches!(
            resolve_mcp(&config, Some(cwd)),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }
}

#[test]
fn in_cwds_rejects_empty_configured_root() {
    let file = scoped_allow_config(&[""]);

    let error = match load_tool_config(file.path()) {
        Ok(_) => panic!("empty root must fail config loading"),
        Err(error) => error,
    };

    assert!(
        error.contains("must not be empty"),
        "unexpected error: {error}"
    );
}

#[test]
fn in_cwds_rejects_relative_configured_root() {
    let file = scoped_allow_config(&["relative/project"]);

    let error = match load_tool_config(file.path()) {
        Ok(_) => panic!("relative root must fail config loading"),
        Err(error) => error,
    };

    assert!(error.contains("absolute"), "unexpected error: {error}");
}

#[test]
fn in_cwds_accepts_nonexistent_absolute_root() {
    let root = format!("/agent-hub-test-nonexistent-{}/project", std::process::id());
    assert!(!std::path::Path::new(&root).exists());
    let file = scoped_allow_config(&[&root]);
    let config = load_tool_config(file.path()).expect("lexical root does not need to exist");

    assert!(matches!(
        resolve_mcp(&config, Some(&format!("{root}/nested"))),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn in_cwds_applies_to_known_non_path_tools_and_arbitrary_mcp_tools() {
    let file = TestConfig::new(
        json!([{
            "tools": ["Bash", "mcp_weather_lookup"],
            "action": "allow",
            "in_cwds": ["/home/user/project"],
        }]),
        "deny",
    );
    let config = load_tool_config(file.path()).expect("config loads");

    for tool in [Tool::Bash, Tool::Unknown("mcp_weather_lookup".to_string())] {
        assert!(matches!(
            resolve_tool(&config, tool, Some("/home/user/project/nested")),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }
}

#[test]
fn in_cwds_expands_tilde_without_filesystem_access() {
    let home = std::env::var("HOME").expect("HOME is available in the test environment");
    let suffix = "agent-hub-in-cwds-tilde-root-that-need-not-exist";
    let file = scoped_allow_config(&[&format!("~/{suffix}")]);
    let config = load_tool_config(file.path()).expect("tilde root expands");

    assert!(matches!(
        resolve_mcp(&config, Some(&format!("{home}/{suffix}/nested"))),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn scoped_deny_precedes_broad_allow_and_preserves_reason() {
    let file = TestConfig::new(
        json!([
            {
                "tools": ["mcp_weather_lookup"],
                "action": "deny",
                "message": "weather MCP is blocked in the sensitive checkout",
                "in_cwds": ["/home/user/sensitive"],
            },
            {
                "tools": ["mcp_weather_lookup"],
                "action": "allow",
            }
        ]),
        "deny",
    );
    let config = load_tool_config(file.path()).expect("config loads");

    match resolve_mcp(&config, Some("/home/user/sensitive/nested")) {
        ConfigAction::Decision(ConfigDecision::Deny(Some(reason))) => {
            assert_eq!(reason, "weather MCP is blocked in the sensitive checkout")
        }
        other => panic!("expected scoped denial with reason, got {other:?}"),
    }
    assert!(matches!(
        resolve_mcp(&config, Some("/home/user/ordinary")),
        ConfigAction::Decision(ConfigDecision::Allow)
    ));
}

#[test]
fn in_cwds_is_a_recognized_rule_field() {
    let file = scoped_allow_config(&["/home/user/project"]);

    let (_, warnings) = validate_tool_config(file.path()).expect("config validates");

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

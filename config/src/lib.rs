pub mod environment;
pub mod image;
pub mod network_policy;
mod rules;
pub mod system;
mod tools;
pub mod triage;

pub use environment::EnvironmentConfig;
pub use image::{ImageConfig, resolve_build_order};
pub use network_policy::{FlattenedPolicy, NetworkPolicyConfig, flatten_policy};
pub use rules::{
    ConfigAction, ConfigDecision, DefaultAction, RuleAction, RuleSummary, ToolConfig,
    default_to_resolved, load_tool_config, resolve_action, validate_tool_config,
};
pub use tools::{is_in_workspace, normalize_path, resolve_path};
pub use system::SystemConfig;
pub use triage::TriagePromptDefinition;

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

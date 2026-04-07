mod rules;
mod tools;

pub use rules::{
    ConfigAction, ConfigDecision, DefaultAction, RuleAction, RuleSummary, ToolConfig,
    default_to_resolved, load_tool_config, resolve_action, validate_tool_config,
};
pub use tools::{is_in_workspace, resolve_path};

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

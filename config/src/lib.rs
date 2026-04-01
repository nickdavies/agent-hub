mod rules;
mod tools;

pub use rules::{
    DefaultAction, ResolvedAction, RuleAction, RuleSummary, ToolConfig, default_to_resolved,
    load_tool_config, resolve_action, validate_tool_config,
};
pub use tools::{
    TOOL_DEFS, ToolCategory, ToolDef, expand_tool_group, find_tool_def, get_matchable_args,
    is_in_workspace, is_path_tool, normalise_tool_name, tools_in_category,
};

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

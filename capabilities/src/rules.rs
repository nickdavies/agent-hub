use serde::{Deserialize, Serialize};

use crate::expand_tilde;
use crate::tools::{expand_tool_group, get_matchable_args, is_in_workspace, is_path_tool};

// --- JSON deserialization types ---

#[derive(Deserialize)]
struct ToolConfigJson {
    #[allow(dead_code)]
    version: u32,
    default: DefaultAction,
    rules: Vec<RuleJson>,
}

#[derive(Deserialize, Serialize)]
struct RuleJson {
    tools: Vec<String>,
    action: RuleAction,
    command: Option<String>,
    message: Option<String>,
    /// Pattern applied to all tools in this rule (e.g. "**/.ssh/**").
    /// Tools with inline patterns like "Write(src/**)" override this.
    pattern: Option<String>,
    /// If set, rule only matches paths inside (true) or outside (false) workspace roots.
    in_workspace: Option<bool>,
    /// If set, rule only matches file-tool calls where ALL resolved paths fall under at least
    /// one of these directory prefixes. Tilde (~) is expanded at load time.
    /// Ignored for non-path tools (Shell, WebFetch, etc.).
    in_paths: Option<Vec<String>>,
}

// --- Compiled config types ---

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum DefaultAction {
    Allow,
    Deny,
    Ask,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
    Delegate,
}

pub(crate) enum ToolPattern {
    Glob(globset::GlobMatcher),
    Regex(regex::Regex),
}

impl ToolPattern {
    fn is_match(&self, value: &str) -> bool {
        match self {
            ToolPattern::Glob(g) => g.is_match(value),
            ToolPattern::Regex(r) => r.is_match(value),
        }
    }
}

pub(crate) struct ToolMatcher {
    pub(crate) name: String,
    pub(crate) pattern: Option<ToolPattern>,
}

impl ToolMatcher {
    fn matches(
        &self,
        tool_name: &str,
        tool_input: Option<&serde_json::Value>,
        cwd: Option<&str>,
    ) -> bool {
        if self.name != tool_name {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(pattern) => {
                let args = tool_input
                    .map(|input| get_matchable_args(tool_name, input, cwd))
                    .unwrap_or_default();
                args.iter().any(|value| pattern.is_match(value))
            }
        }
    }
}

pub(crate) struct Rule {
    pub(crate) matchers: Vec<ToolMatcher>,
    pub(crate) action: RuleAction,
    pub(crate) command: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) in_workspace: Option<bool>,
    pub(crate) in_paths: Option<Vec<String>>,
    pub(crate) source_json: String,
}

pub struct ToolConfig {
    pub default: DefaultAction,
    rules: Vec<Rule>,
}

#[derive(Debug)]
pub enum ResolvedAction {
    Allow,
    Deny(Option<String>),
    Ask,
    Delegate(String),
}

/// Parse a tool entry like "Write", "Write(src/**)", or "Write(regex:\.env)".
pub(crate) fn parse_tool_entry(entry: &str) -> Result<ToolMatcher, String> {
    if let Some(paren_start) = entry.find('(') {
        if !entry.ends_with(')') {
            return Err(format!(
                "malformed tool pattern (missing closing paren): {entry}"
            ));
        }
        let name = &entry[..paren_start];
        let pat = &entry[paren_start + 1..entry.len() - 1];
        if name.is_empty() || pat.is_empty() {
            return Err(format!("empty tool name or pattern: {entry}"));
        }
        let pattern = if let Some(regex_pat) = pat.strip_prefix("regex:") {
            let re = regex::Regex::new(regex_pat)
                .map_err(|e| format!("invalid regex in '{entry}': {e}"))?;
            ToolPattern::Regex(re)
        } else {
            let glob = globset::Glob::new(pat)
                .map_err(|e| format!("invalid glob in '{entry}': {e}"))?
                .compile_matcher();
            ToolPattern::Glob(glob)
        };
        Ok(ToolMatcher {
            name: name.to_string(),
            pattern: Some(pattern),
        })
    } else {
        Ok(ToolMatcher {
            name: entry.to_string(),
            pattern: None,
        })
    }
}

/// Split "Write(src/**)" into ("Write", Some("src/**")), or "Write" into ("Write", None).
fn split_entry(entry: &str) -> (&str, Option<&str>) {
    if let Some(paren_start) = entry.find('(')
        && entry.ends_with(')')
    {
        let name = &entry[..paren_start];
        let pat = &entry[paren_start + 1..entry.len() - 1];
        return (name, Some(pat));
    }
    (entry, None)
}

/// Expand group aliases and apply rule-level patterns, producing entries ready
/// for `parse_tool_entry`. Returns an error if an `@`-prefixed name doesn't
/// match any known group (catches typos like `@Shell` or `@files_write`).
pub(crate) fn expand_tools(
    tools: &[String],
    rule_pattern: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for entry in tools {
        let (name, inline_pat) = split_entry(entry);

        let names: Vec<&str> = if name.starts_with('@') {
            expand_tool_group(name).ok_or_else(|| {
                format!("unknown tool group '{name}' (valid groups: @file, @file_read, @file_write, @shell)")
            })?
        } else {
            vec![name]
        };

        let effective_pat = inline_pat.or(rule_pattern);

        for tool_name in names {
            match effective_pat {
                Some(pat) => result.push(format!("{tool_name}({pat})")),
                None => result.push(tool_name.to_string()),
            }
        }
    }
    Ok(result)
}

pub fn load_tool_config(path: &str) -> Result<ToolConfig, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config {path}: {e}"))?;
    let raw: ToolConfigJson = serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse config {path}: {e}"))?;

    let mut rules = Vec::with_capacity(raw.rules.len());
    for raw_rule in raw.rules {
        if matches!(raw_rule.action, RuleAction::Delegate) && raw_rule.command.is_none() {
            return Err(format!(
                "rule for {:?} has action 'delegate' but no 'command'",
                raw_rule.tools
            ));
        }
        let source_json = serde_json::to_string(&raw_rule).unwrap_or_default();
        let expanded = expand_tools(&raw_rule.tools, raw_rule.pattern.as_deref())?;
        let matchers = expanded
            .iter()
            .map(|t| parse_tool_entry(t))
            .collect::<Result<Vec<_>, _>>()?;
        rules.push(Rule {
            matchers,
            action: raw_rule.action,
            command: raw_rule.command,
            message: raw_rule.message,
            in_workspace: raw_rule.in_workspace,
            in_paths: raw_rule
                .in_paths
                .map(|paths| paths.into_iter().map(|p| expand_tilde(&p)).collect()),
            source_json,
        });
    }

    Ok(ToolConfig {
        default: raw.default,
        rules,
    })
}

pub fn resolve_action(
    config: &ToolConfig,
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    cwd: Option<&str>,
    workspace_roots: Option<&[String]>,
) -> ResolvedAction {
    // For path-based tools, resolve paths and deny if any is not absolute
    let resolved_paths: Vec<String> = if is_path_tool(tool_name) {
        tool_input
            .map(|input| get_matchable_args(tool_name, input, cwd))
            .unwrap_or_default()
    } else {
        vec![]
    };

    if resolved_paths.iter().any(|p| !p.starts_with('/')) {
        return ResolvedAction::Deny(Some(
            "path-based tool arguments must be absolute paths".to_string(),
        ));
    }

    // Pre-compute workspace membership (None if no paths or no workspace_roots).
    // For path-based tools with no explicit path arg, fall back to cwd.
    let path_in_workspace: Option<bool> = if !resolved_paths.is_empty() {
        workspace_roots.map(|roots| resolved_paths.iter().all(|p| is_in_workspace(p, roots)))
    } else if is_path_tool(tool_name) {
        match (cwd, workspace_roots) {
            (Some(cwd), Some(roots)) => Some(is_in_workspace(cwd, roots)),
            _ => None,
        }
    } else {
        None
    };

    eprintln!(
        "agent-hub-gateway: [debug] tool={tool_name} workspace_roots={workspace_roots:?} path_in_workspace={path_in_workspace:?}"
    );

    for (i, rule) in config.rules.iter().enumerate() {
        if let Some(required) = rule.in_workspace {
            match path_in_workspace {
                Some(actual) if actual != required => continue,
                None => continue,
                _ => {}
            }
        }

        if let Some(in_paths) = &rule.in_paths
            && is_path_tool(tool_name)
        {
            if resolved_paths.is_empty() {
                // No path extracted — can't check in_paths, skip rule.
                continue;
            }
            let all_in_paths = resolved_paths.iter().all(|p| is_in_workspace(p, in_paths));
            if !all_in_paths {
                continue;
            }
        }
        // Non-path tools: in_paths is ignored, rule applies normally.

        if rule
            .matchers
            .iter()
            .any(|m| m.matches(tool_name, tool_input, cwd))
        {
            eprintln!(
                "agent-hub-gateway: [debug] matched rule[{i}]: {}",
                rule.source_json
            );
            return match &rule.action {
                RuleAction::Allow => ResolvedAction::Allow,
                RuleAction::Deny => ResolvedAction::Deny(rule.message.clone()),
                RuleAction::Ask => ResolvedAction::Ask,
                RuleAction::Delegate => ResolvedAction::Delegate(rule.command.clone().unwrap()),
            };
        }
    }
    eprintln!("agent-hub-gateway: [debug] no rule matched, using default");
    default_to_resolved(&config.default)
}

pub fn default_to_resolved(default: &DefaultAction) -> ResolvedAction {
    match default {
        DefaultAction::Allow => ResolvedAction::Allow,
        DefaultAction::Deny => ResolvedAction::Deny(None),
        DefaultAction::Ask => ResolvedAction::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCategory, tools_in_category};

    // --- parse_tool_entry ---

    #[test]
    fn parse_bare_name() {
        let m = parse_tool_entry("Write").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_none());
    }

    #[test]
    fn parse_glob_pattern() {
        let m = parse_tool_entry("Write(src/**)").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_some());
    }

    #[test]
    fn parse_regex_pattern() {
        let m = parse_tool_entry(r"Write(regex:\.env)").unwrap();
        assert_eq!(m.name, "Write");
        assert!(m.pattern.is_some());
    }

    #[test]
    fn parse_malformed_entries() {
        assert!(parse_tool_entry("Write(src/**").is_err());
        assert!(parse_tool_entry("(src/**)").is_err());
        assert!(parse_tool_entry("Write()").is_err());
    }

    #[test]
    fn parse_invalid_regex() {
        assert!(parse_tool_entry("Write(regex:[invalid)").is_err());
    }

    // --- ToolMatcher::matches ---

    #[test]
    fn bare_name_matches_any_input() {
        let m = parse_tool_entry("Write").unwrap();
        let input = serde_json::json!({"path": "/any/path"});
        assert!(m.matches("Write", Some(&input), None));
        assert!(m.matches("Write", None, None));
    }

    #[test]
    fn bare_name_rejects_wrong_tool() {
        let m = parse_tool_entry("Write").unwrap();
        assert!(!m.matches("Read", None, None));
    }

    #[test]
    fn glob_matches_path() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        let yes = serde_json::json!({"path": "/src/foo/bar.rs"});
        let no = serde_json::json!({"path": "/tests/foo.rs"});
        assert!(m.matches("Write", Some(&yes), None));
        assert!(!m.matches("Write", Some(&no), None));
    }

    #[test]
    fn glob_no_input_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        assert!(!m.matches("Write", None, None));
    }

    #[test]
    fn glob_missing_field_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        let input = serde_json::json!({"other_field": "/src/foo.rs"});
        assert!(!m.matches("Write", Some(&input), None));
    }

    #[test]
    fn regex_matches_path() {
        let m = parse_tool_entry(r"Write(regex:\.env)").unwrap();
        let yes = serde_json::json!({"path": "/home/user/.env"});
        let no = serde_json::json!({"path": "/src/main.rs"});
        assert!(m.matches("Write", Some(&yes), None));
        assert!(!m.matches("Write", Some(&no), None));
    }

    #[test]
    fn glob_matches_command_field() {
        let m = parse_tool_entry("Bash(npm *)").unwrap();
        let yes = serde_json::json!({"command": "npm test"});
        let no = serde_json::json!({"command": "cargo test"});
        assert!(m.matches("Bash", Some(&yes), None));
        assert!(!m.matches("Bash", Some(&no), None));
    }

    #[test]
    fn glob_matches_url_field() {
        let m = parse_tool_entry("WebFetch(https://example.com/**)").unwrap();
        let yes = serde_json::json!({"url": "https://example.com/page"});
        let no = serde_json::json!({"url": "https://other.com/page"});
        assert!(m.matches("WebFetch", Some(&yes), None));
        assert!(!m.matches("WebFetch", Some(&no), None));
    }

    // --- resolve_action (end-to-end) ---

    fn make_config(rules: Vec<Rule>, default: DefaultAction) -> ToolConfig {
        ToolConfig { default, rules }
    }

    fn make_rule(entries: &[&str], action: RuleAction) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
            command: None,
            message: None,
            in_workspace: None,
            in_paths: None,
            source_json: String::new(),
        }
    }

    fn make_deny_rule(entries: &[&str], message: &str) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action: RuleAction::Deny,
            command: None,
            message: Some(message.to_string()),
            in_workspace: None,
            in_paths: None,
            source_json: String::new(),
        }
    }

    fn make_workspace_rule(
        entries: &[&str],
        action: RuleAction,
        in_workspace: Option<bool>,
    ) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
            command: None,
            message: None,
            in_workspace,
            in_paths: None,
            source_json: String::new(),
        }
    }

    #[test]
    fn deny_pattern_before_allow_bare() {
        let config = make_config(
            vec![
                make_rule(&["Write(**/.env*)"], RuleAction::Deny),
                make_rule(&["Write"], RuleAction::Allow),
            ],
            DefaultAction::Ask,
        );

        let env = serde_json::json!({"path": "/config/.env.local"});
        let normal = serde_json::json!({"path": "/src/main.rs"});

        assert!(matches!(
            resolve_action(&config, "Write", Some(&env), None, None),
            ResolvedAction::Deny(_)
        ));
        assert!(matches!(
            resolve_action(&config, "Write", Some(&normal), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn unmatched_tool_falls_to_default() {
        let config = make_config(
            vec![make_rule(&["Write"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Read", None, None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn multiple_matchers_in_single_rule() {
        let config = make_config(
            vec![make_rule(&["Read", "Grep", "Glob"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Read", None, None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Grep", None, None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Write", None, None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn pattern_with_no_input_skips_rule() {
        let config = make_config(
            vec![
                make_rule(&["Write(/src/**)"], RuleAction::Allow),
                make_rule(&["Write"], RuleAction::Deny),
            ],
            DefaultAction::Ask,
        );
        // No tool_input: pattern rule can't match, falls through to bare "Write" -> Deny
        assert!(matches!(
            resolve_action(&config, "Write", None, None, None),
            ResolvedAction::Deny(_)
        ));
    }

    // --- absolute path enforcement ---

    #[test]
    fn relative_path_without_cwd_is_denied() {
        let config = make_config(
            vec![make_rule(&["Read"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "relative/path.txt"});
        match resolve_action(&config, "Read", Some(&input), None, None) {
            ResolvedAction::Deny(Some(msg)) => {
                assert!(
                    msg.contains("absolute"),
                    "expected absolute path error: {msg}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn relative_path_resolved_by_cwd_is_allowed() {
        let config = make_config(
            vec![make_rule(&["Read"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "src/main.rs"});
        assert!(matches!(
            resolve_action(
                &config,
                "Read",
                Some(&input),
                Some("/home/user/project"),
                None
            ),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_shell() {
        let config = make_config(
            vec![make_rule(&["Bash"], RuleAction::Allow)],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"command": "ls relative/path"});
        assert!(matches!(
            resolve_action(&config, "Bash", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_unknown_tools() {
        let config = make_config(vec![], DefaultAction::Allow);
        let input = serde_json::json!({"whatever": "relative/path"});
        assert!(matches!(
            resolve_action(&config, "UnknownTool", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    // --- deny messages ---

    #[test]
    fn deny_rule_with_message() {
        let config = make_config(
            vec![make_deny_rule(&["Delete"], "Use trash instead of delete")],
            DefaultAction::Ask,
        );
        match resolve_action(&config, "Delete", None, None, None) {
            ResolvedAction::Deny(Some(msg)) => {
                assert_eq!(msg, "Use trash instead of delete");
            }
            other => panic!("expected Deny with message, got {other:?}"),
        }
    }

    #[test]
    fn deny_rule_without_message() {
        let config = make_config(
            vec![make_rule(&["Delete"], RuleAction::Deny)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action(&config, "Delete", None, None, None),
            ResolvedAction::Deny(None)
        ));
    }

    #[test]
    fn deny_default_has_no_message() {
        let config = make_config(vec![], DefaultAction::Deny);
        assert!(matches!(
            resolve_action(&config, "Write", None, None, None),
            ResolvedAction::Deny(None)
        ));
    }

    // --- cwd + pattern matching integration ---

    #[test]
    fn cwd_resolved_path_matches_pattern() {
        let config = make_config(
            vec![
                make_rule(&[r"Read(regex:\.ssh)"], RuleAction::Deny),
                make_rule(&["Read"], RuleAction::Allow),
            ],
            DefaultAction::Ask,
        );
        // Relative path "id_rsa" with cwd ~/.ssh -> resolved to /home/user/.ssh/id_rsa
        let input = serde_json::json!({"path": "id_rsa"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), Some("/home/user/.ssh"), None),
            ResolvedAction::Deny(_)
        ));
    }

    // --- expand_tools / group aliases ---

    #[test]
    fn expand_file_group_bare() {
        let file_read = tools_in_category(ToolCategory::FileRead);
        let file_write = tools_in_category(ToolCategory::FileWrite);
        let tools = vec!["@file".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(expanded.len(), file_read.len() + file_write.len());
        assert!(expanded.contains(&"Read".to_string()));
        assert!(expanded.contains(&"Write".to_string()));
        assert!(expanded.contains(&"Delete".to_string()));
    }

    #[test]
    fn expand_file_read_group() {
        let tools = vec!["@file_read".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(
            expanded.len(),
            tools_in_category(ToolCategory::FileRead).len()
        );
        assert!(expanded.contains(&"Read".to_string()));
        assert!(!expanded.contains(&"Write".to_string()));
    }

    #[test]
    fn expand_file_write_group() {
        let tools = vec!["@file_write".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(
            expanded.len(),
            tools_in_category(ToolCategory::FileWrite).len()
        );
        assert!(expanded.contains(&"Write".to_string()));
        assert!(expanded.contains(&"StrReplace".to_string()));
        assert!(!expanded.contains(&"Read".to_string()));
    }

    #[test]
    fn expand_file_group_with_inline_pattern() {
        let file_read = tools_in_category(ToolCategory::FileRead);
        let file_write = tools_in_category(ToolCategory::FileWrite);
        let tools = vec!["@file(**/.ssh/**)".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(expanded.len(), file_read.len() + file_write.len());
        assert!(expanded.contains(&"Read(**/.ssh/**)".to_string()));
        assert!(expanded.contains(&"Write(**/.ssh/**)".to_string()));
    }

    #[test]
    fn expand_file_group_with_rule_pattern() {
        let tools = vec!["@file".to_string()];
        let expanded = expand_tools(&tools, Some("**/.env*")).unwrap();
        assert!(expanded.contains(&"Read(**/.env*)".to_string()));
        assert!(expanded.contains(&"Write(**/.env*)".to_string()));
    }

    #[test]
    fn expand_shell_group() {
        let tools = vec!["@shell".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(expanded, vec!["Bash", "Shell"]);
    }

    #[test]
    fn expand_mixed_groups_and_names() {
        let file_read = tools_in_category(ToolCategory::FileRead);
        let file_write = tools_in_category(ToolCategory::FileWrite);
        let tools = vec!["@file".to_string(), "WebFetch".to_string()];
        let expanded = expand_tools(&tools, None).unwrap();
        assert_eq!(expanded.len(), file_read.len() + file_write.len() + 1);
        assert!(expanded.contains(&"WebFetch".to_string()));
    }

    #[test]
    fn rule_pattern_applied_to_bare_names() {
        let tools = vec!["Read".to_string(), "Write".to_string()];
        let expanded = expand_tools(&tools, Some("**/.ssh/**")).unwrap();
        assert_eq!(expanded, vec!["Read(**/.ssh/**)", "Write(**/.ssh/**)"]);
    }

    #[test]
    fn inline_pattern_overrides_rule_pattern() {
        let tools = vec!["Read(/src/**)".to_string(), "Write".to_string()];
        let expanded = expand_tools(&tools, Some("**/.ssh/**")).unwrap();
        assert_eq!(expanded, vec!["Read(/src/**)", "Write(**/.ssh/**)"]);
    }

    #[test]
    fn expand_unknown_group_is_error() {
        let tools = vec!["@Shell".to_string()];
        assert!(expand_tools(&tools, None).is_err());

        let tools = vec!["@files_write".to_string()];
        assert!(expand_tools(&tools, None).is_err());
    }

    // --- end-to-end with groups ---

    #[test]
    fn file_group_deny_ssh() {
        let config = make_config(
            vec![
                Rule {
                    matchers: expand_tools(&["@file".to_string()], Some(r"regex:\.ssh"))
                        .expect("valid group")
                        .iter()
                        .map(|t| parse_tool_entry(t).expect("valid entry"))
                        .collect(),
                    action: RuleAction::Deny,
                    command: None,
                    message: Some("Access to .ssh is not allowed".into()),
                    in_workspace: None,
                    in_paths: None,
                    source_json: String::new(),
                },
                make_rule(&["Read", "Write"], RuleAction::Allow),
            ],
            DefaultAction::Ask,
        );

        let ssh_input = serde_json::json!({"path": "/home/user/.ssh/id_rsa"});
        let normal_input = serde_json::json!({"path": "/home/user/project/main.rs"});

        match resolve_action(&config, "Read", Some(&ssh_input), None, None) {
            ResolvedAction::Deny(Some(msg)) => assert!(msg.contains(".ssh")),
            other => panic!("expected Deny for .ssh read, got {other:?}"),
        }
        assert!(matches!(
            resolve_action(&config, "Write", Some(&ssh_input), None, None),
            ResolvedAction::Deny(_)
        ));
        assert!(matches!(
            resolve_action(&config, "Read", Some(&normal_input), None, None),
            ResolvedAction::Allow
        ));
    }

    // --- in_workspace rule filter ---

    #[test]
    fn in_workspace_true_allows_workspace_paths() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"path": "/home/user/project/src/main.rs"});

        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, Some(&roots)),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn in_workspace_true_skips_outside_paths() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"path": "/etc/passwd"});

        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, Some(&roots)),
            ResolvedAction::Deny(_)
        ));
    }

    #[test]
    fn in_workspace_false_matches_outside_paths() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Write"], RuleAction::Ask, Some(false)),
                make_workspace_rule(&["Write"], RuleAction::Allow, None),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        let outside = serde_json::json!({"path": "/tmp/scratch.txt"});
        let inside = serde_json::json!({"path": "/home/user/project/src/main.rs"});

        assert!(matches!(
            resolve_action(&config, "Write", Some(&outside), None, Some(&roots)),
            ResolvedAction::Ask
        ));
        assert!(matches!(
            resolve_action(&config, "Write", Some(&inside), None, Some(&roots)),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn in_workspace_skipped_for_non_path_tools() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Bash"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"command": "ls"});

        // Can't determine workspace membership for Bash, workspace rule skipped, falls to default
        assert!(matches!(
            resolve_action(&config, "Bash", Some(&input), None, Some(&roots)),
            ResolvedAction::Deny(_)
        ));
    }

    #[test]
    fn in_workspace_skipped_without_roots() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let input = serde_json::json!({"path": "/home/user/project/src/main.rs"});

        // No workspace_roots provided, workspace rule skipped, falls to default
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Deny(_)
        ));
    }

    #[test]
    fn in_workspace_combined_with_deny_pattern() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Write(**/.git/**)"], RuleAction::Deny, Some(true)),
                make_workspace_rule(&["Write"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["Write"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        let git_file = serde_json::json!({"path": "/home/user/project/.git/config"});
        let src_file = serde_json::json!({"path": "/home/user/project/src/main.rs"});
        let outside = serde_json::json!({"path": "/tmp/scratch.txt"});

        assert!(matches!(
            resolve_action(&config, "Write", Some(&git_file), None, Some(&roots)),
            ResolvedAction::Deny(_)
        ));
        assert!(matches!(
            resolve_action(&config, "Write", Some(&src_file), None, Some(&roots)),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Write", Some(&outside), None, Some(&roots)),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn dotdot_traversal_not_in_workspace() {
        // Verify that ../ traversal out of workspace is correctly detected
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"path": "../.ssh/id_rsa"});

        // With cwd inside workspace, ../ resolves outside => not in workspace => denied
        assert!(matches!(
            resolve_action(
                &config,
                "Read",
                Some(&input),
                Some("/home/user/project"),
                Some(&roots)
            ),
            ResolvedAction::Deny(_)
        ));
    }

    // --- search tools end-to-end ---

    #[test]
    fn grep_outside_workspace_triggers_in_workspace_rule() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        let inside = serde_json::json!({"pattern": "TODO", "path": "/home/user/project/src"});
        let outside = serde_json::json!({"pattern": "secret", "path": "/home/user/.ssh"});

        assert!(matches!(
            resolve_action(&config, "Grep", Some(&inside), None, Some(&roots)),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Grep", Some(&outside), None, Some(&roots)),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn grep_without_path_or_cwd_skips_workspace_rules() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"pattern": "TODO"});

        // No path arg AND no cwd means no workspace determination, both rules skipped
        assert!(matches!(
            resolve_action(&config, "Grep", Some(&input), None, Some(&roots)),
            ResolvedAction::Deny(_)
        ));
    }

    #[test]
    fn grep_without_path_falls_back_to_cwd_for_workspace() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"pattern": "TODO"});

        // No path arg but cwd is inside workspace => falls back to cwd
        assert!(matches!(
            resolve_action(
                &config,
                "Grep",
                Some(&input),
                Some("/home/user/project"),
                Some(&roots)
            ),
            ResolvedAction::Allow
        ));

        // No path arg but cwd is outside workspace
        assert!(matches!(
            resolve_action(&config, "Grep", Some(&input), Some("/tmp"), Some(&roots)),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn semantic_search_all_dirs_in_workspace() {
        let config = make_config(
            vec![make_workspace_rule(
                &["SemanticSearch"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({
            "query": "auth",
            "target_directories": ["/home/user/project/src", "/home/user/project/lib"]
        });

        assert!(matches!(
            resolve_action(&config, "SemanticSearch", Some(&input), None, Some(&roots)),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn semantic_search_one_dir_outside_workspace_taints() {
        let config = make_config(
            vec![
                make_workspace_rule(&["SemanticSearch"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["SemanticSearch"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({
            "query": "secrets",
            "target_directories": ["/home/user/project/src", "/home/user/.ssh"]
        });

        // One dir outside workspace => not in_workspace => Ask
        assert!(matches!(
            resolve_action(&config, "SemanticSearch", Some(&input), None, Some(&roots)),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn semantic_search_empty_dirs_skips_workspace_rules() {
        let config = make_config(
            vec![make_workspace_rule(
                &["SemanticSearch"],
                RuleAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];
        let input = serde_json::json!({"query": "auth", "target_directories": []});

        // Empty dirs = can't determine workspace, rule skipped
        assert!(matches!(
            resolve_action(&config, "SemanticSearch", Some(&input), None, Some(&roots)),
            ResolvedAction::Deny(_)
        ));
    }

    // --- in_paths ---

    fn make_in_paths_rule(entries: &[&str], dirs: &[&str], action: RuleAction) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
            command: None,
            message: None,
            in_workspace: None,
            in_paths: Some(dirs.iter().map(|s| s.to_string()).collect()),
            source_json: String::new(),
        }
    }

    #[test]
    fn in_paths_allows_matching_path() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "/home/user/oss/cilium/main.go"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn in_paths_skips_non_matching_path() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "/etc/passwd"});
        // Rule skipped, falls to default Ask
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_no_path_extracted_skips_rule() {
        let config = make_config(
            vec![
                make_in_paths_rule(&["Read"], &["/home/user/oss"], RuleAction::Allow),
                make_rule(&["Read"], RuleAction::Deny),
            ],
            DefaultAction::Ask,
        );
        // No path field in input — can't check in_paths, rule skipped, falls to next rule
        let input = serde_json::json!({"other": "value"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Deny(_)
        ));
    }

    #[test]
    fn in_paths_semantic_search_all_in_paths() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["SemanticSearch"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({
            "query": "auth",
            "target_directories": ["/home/user/oss/cilium/src", "/home/user/oss/linux/net"]
        });
        assert!(matches!(
            resolve_action(&config, "SemanticSearch", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn in_paths_semantic_search_mixed_skips_rule() {
        let config = make_config(
            vec![
                make_in_paths_rule(&["SemanticSearch"], &["/home/user/oss"], RuleAction::Allow),
                make_rule(&["SemanticSearch"], RuleAction::Ask),
            ],
            DefaultAction::Deny,
        );
        // One dir inside in_paths, one not — rule skipped, falls to next rule (Ask)
        let input = serde_json::json!({
            "query": "secrets",
            "target_directories": ["/home/user/oss/cilium/src", "/home/user/.ssh"]
        });
        assert!(matches!(
            resolve_action(&config, "SemanticSearch", Some(&input), None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_multiple_dirs() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss", "/home/user/workspaces"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let oss = serde_json::json!({"path": "/home/user/oss/cilium/main.go"});
        let ws = serde_json::json!({"path": "/home/user/workspaces/project/src/lib.rs"});
        let other = serde_json::json!({"path": "/home/user/.ssh/id_rsa"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&oss), None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Read", Some(&ws), None, None),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Read", Some(&other), None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_composes_with_in_workspace() {
        // Rule requires BOTH in_workspace=true AND in_paths
        let config = make_config(
            vec![Rule {
                matchers: vec![parse_tool_entry("Read").unwrap()],
                action: RuleAction::Allow,
                command: None,
                message: None,
                in_workspace: Some(true),
                in_paths: Some(vec!["/home/user/oss".to_string()]),
                source_json: String::new(),
            }],
            DefaultAction::Ask,
        );
        let roots = vec!["/home/user/oss/cilium".to_string()];

        // Inside workspace AND inside in_paths → Allow
        let inside_in_paths = serde_json::json!({"path": "/home/user/oss/cilium/main.go"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&inside_in_paths), None, Some(&roots)),
            ResolvedAction::Allow
        ));

        // Inside workspace but NOT inside in_paths → rule skipped → Ask
        let inside_not_in_paths =
            serde_json::json!({"path": "/home/user/oss/cilium/../other/secret.txt"});
        assert!(matches!(
            resolve_action(
                &config,
                "Read",
                Some(&inside_not_in_paths),
                None,
                Some(&roots)
            ),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_tilde_expansion() {
        // Simulate what load_tool_config does: expand_tilde at load time.
        // We build the rule manually with the already-expanded path, matching
        // what HOME would be set to in the test environment.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let expanded = format!("{home}/oss");
        let config = make_config(
            vec![Rule {
                matchers: vec![parse_tool_entry("Read").unwrap()],
                action: RuleAction::Allow,
                command: None,
                message: None,
                in_workspace: None,
                in_paths: Some(vec![expanded.clone()]),
                source_json: String::new(),
            }],
            DefaultAction::Ask,
        );
        let path = format!("{home}/oss/cilium/main.go");
        let input = serde_json::json!({"path": path});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn in_paths_prefix_not_confused_with_dir() {
        // "/home/user/oss-other" should NOT match in_paths "/home/user/oss"
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "/home/user/oss-other/main.go"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_dotdot_traversal_blocked() {
        // /home/user/oss/../unsafe normalises to /home/user/unsafe — must NOT match in_paths /home/user/oss
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "/home/user/oss/../unsafe/secret.txt"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Ask
        ));
    }

    #[test]
    fn in_paths_dotdot_within_dir_allowed() {
        // /home/user/oss/cilium/../linux is still inside /home/user/oss — should be allowed
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                RuleAction::Allow,
            )],
            DefaultAction::Ask,
        );
        let input = serde_json::json!({"path": "/home/user/oss/cilium/../linux/net/core.c"});
        assert!(matches!(
            resolve_action(&config, "Read", Some(&input), None, None),
            ResolvedAction::Allow
        ));
    }

    #[test]
    fn glob_in_workspace_check() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Glob"], RuleAction::Allow, Some(true)),
                make_workspace_rule(&["Glob"], RuleAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        let inside = serde_json::json!({"glob_pattern": "*.rs", "target_directory": "/home/user/project/src"});
        let outside =
            serde_json::json!({"glob_pattern": "id_*", "target_directory": "/home/user/.ssh"});

        assert!(matches!(
            resolve_action(&config, "Glob", Some(&inside), None, Some(&roots)),
            ResolvedAction::Allow
        ));
        assert!(matches!(
            resolve_action(&config, "Glob", Some(&outside), None, Some(&roots)),
            ResolvedAction::Ask
        ));
    }
}

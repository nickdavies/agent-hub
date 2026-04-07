use serde::{Deserialize, Serialize};
use strum::Display;

use protocol::{Tool, expand_tool_group};

use crate::expand_tilde;
use crate::tools::is_in_workspace;

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

/// Rule action as it appears in the JSON config file.
///
/// For `Delegate` rules, the command is a separate `"command"` field in the JSON
/// object — it is combined into [`ResolvedAction::Delegate`] at load time.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Deny,
    Ask,
    Delegate,
}

/// Compiled rule action with all required data embedded.
///
/// Unlike [`RuleAction`] (which mirrors the JSON config), this enum guarantees
/// that `Delegate` always carries a non-empty command string — enforced at
/// config load time. No `.unwrap()` needed at match time.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAction {
    Allow,
    Deny,
    Ask,
    Delegate(String),
}

impl ResolvedAction {
    /// The action variant as a [`RuleAction`] (for display/summary purposes).
    pub fn action_kind(&self) -> RuleAction {
        match self {
            ResolvedAction::Allow => RuleAction::Allow,
            ResolvedAction::Deny => RuleAction::Deny,
            ResolvedAction::Ask => RuleAction::Ask,
            ResolvedAction::Delegate(_) => RuleAction::Delegate,
        }
    }

    /// The delegate command, if this is a `Delegate` action.
    pub fn command(&self) -> Option<&str> {
        match self {
            ResolvedAction::Delegate(cmd) => Some(cmd),
            _ => None,
        }
    }
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
    fn matches(&self, tool_name: &str, resolved_args: &[String]) -> bool {
        if self.name != tool_name {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(pattern) => resolved_args.iter().any(|value| pattern.is_match(value)),
        }
    }
}

pub(crate) struct Rule {
    pub(crate) matchers: Vec<ToolMatcher>,
    pub(crate) action: ResolvedAction,
    pub(crate) message: Option<String>,
    pub(crate) in_workspace: Option<bool>,
    pub(crate) in_paths: Option<Vec<String>>,
    pub(crate) source_json: String,
}

pub struct ToolConfig {
    pub default: DefaultAction,
    rules: Vec<Rule>,
}

/// The three possible local decisions from config rule evaluation.
/// Also the only outcomes a delegate subprocess can return (never another delegation).
#[derive(Debug)]
pub enum ConfigDecision {
    Allow,
    Deny(Option<String>),
    Ask,
}

/// Result of resolving a tool call against the config rules.
#[derive(Debug)]
pub enum ConfigAction {
    /// A decision that can be acted on directly.
    Decision(ConfigDecision),
    /// Delegate to an external subprocess for the decision.
    Delegation(String),
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
        let source_json = serde_json::to_string(&raw_rule).unwrap_or_default();
        let expanded = expand_tools(&raw_rule.tools, raw_rule.pattern.as_deref())?;
        let matchers = expanded
            .iter()
            .map(|t| parse_tool_entry(t))
            .collect::<Result<Vec<_>, _>>()?;
        let resolved_action = match raw_rule.action {
            RuleAction::Allow => ResolvedAction::Allow,
            RuleAction::Deny => ResolvedAction::Deny,
            RuleAction::Ask => ResolvedAction::Ask,
            RuleAction::Delegate => {
                let cmd = raw_rule.command.ok_or_else(|| {
                    format!(
                        "rule for {:?} has action 'delegate' but no 'command'",
                        raw_rule.tools
                    )
                })?;
                ResolvedAction::Delegate(cmd)
            }
        };
        rules.push(Rule {
            matchers,
            action: resolved_action,
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
    tool: &Tool,
    resolved_args: &[String],
    cwd: Option<&str>,
    workspace_roots: Option<&[String]>,
) -> ConfigAction {
    let tool_name = tool.as_str();
    let is_path = tool.is_path_tool();

    // For path-based tools, resolved_args should already contain resolved paths.
    // Deny if any path is not absolute (caller must resolve relative paths first).
    let resolved_paths: &[String] = if is_path { resolved_args } else { &[] };

    if resolved_paths.iter().any(|p| !p.starts_with('/')) {
        return ConfigAction::Decision(ConfigDecision::Deny(Some(
            "path-based tool arguments must be absolute paths".to_string(),
        )));
    }

    // Pre-compute workspace membership (None if no paths or no workspace_roots).
    // For path-based tools with no explicit path arg, fall back to cwd.
    let path_in_workspace: Option<bool> = if !resolved_paths.is_empty() {
        workspace_roots.map(|roots| resolved_paths.iter().all(|p| is_in_workspace(p, roots)))
    } else if is_path {
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
            && is_path
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
            .any(|m| m.matches(tool_name, resolved_args))
        {
            eprintln!(
                "agent-hub-gateway: [debug] matched rule[{i}]: {}",
                rule.source_json
            );
            return match &rule.action {
                ResolvedAction::Allow => ConfigAction::Decision(ConfigDecision::Allow),
                ResolvedAction::Deny => {
                    ConfigAction::Decision(ConfigDecision::Deny(rule.message.clone()))
                }
                ResolvedAction::Ask => ConfigAction::Decision(ConfigDecision::Ask),
                ResolvedAction::Delegate(command) => ConfigAction::Delegation(command.clone()),
            };
        }
    }
    eprintln!("agent-hub-gateway: [debug] no rule matched, using default");
    default_to_resolved(&config.default)
}

pub fn default_to_resolved(default: &DefaultAction) -> ConfigAction {
    match default {
        DefaultAction::Allow => ConfigAction::Decision(ConfigDecision::Allow),
        DefaultAction::Deny => ConfigAction::Decision(ConfigDecision::Deny(None)),
        DefaultAction::Ask => ConfigAction::Decision(ConfigDecision::Ask),
    }
}

// --- Validation types ---

/// Summary of a single rule for validation/display.
pub struct RuleSummary {
    pub index: usize,
    pub tools: Vec<String>,
    pub action: RuleAction,
    pub command: Option<String>,
    pub source_json: String,
}

impl ToolConfig {
    /// Return summaries of all rules for validation/display purposes.
    pub fn rule_summaries(&self) -> Vec<RuleSummary> {
        self.rules
            .iter()
            .enumerate()
            .map(|(i, rule)| RuleSummary {
                index: i,
                tools: rule.matchers.iter().map(|m| m.name.clone()).collect(),
                action: rule.action.action_kind(),
                command: rule.action.command().map(str::to_owned),
                source_json: rule.source_json.clone(),
            })
            .collect()
    }
}

/// Validate a config file with stricter checks than `load_tool_config`.
/// Returns `(config, warnings)` on success, or an error string on failure.
///
/// In addition to the standard `load_tool_config` checks (JSON syntax, schema,
/// group expansion, pattern compilation, delegate-without-command), this function
/// detects:
/// - Unknown top-level fields (typos like `"defualt"`)
/// - Unknown per-rule fields
/// - Non-delegate rules that have a `"command"` field (likely a mistake)
pub fn validate_tool_config(path: &str) -> Result<(ToolConfig, Vec<String>), String> {
    let config = load_tool_config(path)?;
    let mut warnings = Vec::new();

    // Re-read and parse as raw JSON to detect unknown fields
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config {path}: {e}"))?;
    let raw: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse config {path}: {e}"))?;

    // Check top-level fields
    let known_top = ["version", "default", "rules"];
    if let Some(obj) = raw.as_object() {
        for key in obj.keys() {
            if !known_top.contains(&key.as_str()) {
                warnings.push(format!("unknown top-level field \"{key}\""));
            }
        }
    }

    // Check per-rule fields
    let known_rule = [
        "tools",
        "action",
        "command",
        "message",
        "pattern",
        "in_workspace",
        "in_paths",
    ];
    if let Some(rules) = raw.get("rules").and_then(|r| r.as_array()) {
        for (i, rule) in rules.iter().enumerate() {
            if let Some(obj) = rule.as_object() {
                for key in obj.keys() {
                    if !known_rule.contains(&key.as_str()) {
                        warnings.push(format!("rule {}: unknown field \"{key}\"", i + 1));
                    }
                }
                // Warn on non-delegate rule with command field
                let is_delegate = obj.get("action").and_then(|a| a.as_str()) == Some("delegate");
                if !is_delegate && obj.contains_key("command") {
                    warnings.push(format!(
                        "rule {}: has \"command\" but action is not \"delegate\"",
                        i + 1
                    ));
                }
            }
        }
    }

    Ok((config, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ToolCategory;

    /// Test convenience: parse a tool name string into a `Tool` and call `resolve_action`.
    /// Keeps test call sites concise without changing every occurrence.
    fn resolve_action_by_name(
        config: &ToolConfig,
        tool_name: &str,
        resolved_args: &[String],
        cwd: Option<&str>,
        workspace_roots: Option<&[String]>,
    ) -> ConfigAction {
        let tool: Tool = serde_json::from_value(serde_json::json!(tool_name)).unwrap();
        resolve_action(config, &tool, resolved_args, cwd, workspace_roots)
    }

    fn tools_in_category(cat: ToolCategory) -> &'static [&'static str] {
        Tool::tools_in_category(cat)
    }

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
        assert!(m.matches("Write", &["/any/path".to_string()]));
        assert!(m.matches("Write", &[]));
    }

    #[test]
    fn bare_name_rejects_wrong_tool() {
        let m = parse_tool_entry("Write").unwrap();
        assert!(!m.matches("Read", &[]));
    }

    #[test]
    fn glob_matches_path() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        assert!(m.matches("Write", &["/src/foo/bar.rs".to_string()]));
        assert!(!m.matches("Write", &["/tests/foo.rs".to_string()]));
    }

    #[test]
    fn glob_no_input_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        assert!(!m.matches("Write", &[]));
    }

    #[test]
    fn glob_missing_field_is_no_match() {
        let m = parse_tool_entry("Write(/src/**)").unwrap();
        // No matchable args extracted — pattern can't match
        assert!(!m.matches("Write", &[]));
    }

    #[test]
    fn regex_matches_path() {
        let m = parse_tool_entry(r"Write(regex:\.env)").unwrap();
        assert!(m.matches("Write", &["/home/user/.env".to_string()]));
        assert!(!m.matches("Write", &["/src/main.rs".to_string()]));
    }

    #[test]
    fn glob_matches_command_field() {
        let m = parse_tool_entry("Bash(npm *)").unwrap();
        assert!(m.matches("Bash", &["npm test".to_string()]));
        assert!(!m.matches("Bash", &["cargo test".to_string()]));
    }

    #[test]
    fn glob_matches_url_field() {
        let m = parse_tool_entry("WebFetch(https://example.com/**)").unwrap();
        assert!(m.matches("WebFetch", &["https://example.com/page".to_string()]));
        assert!(!m.matches("WebFetch", &["https://other.com/page".to_string()]));
    }

    // --- resolve_action (end-to-end) ---

    fn make_config(rules: Vec<Rule>, default: DefaultAction) -> ToolConfig {
        ToolConfig { default, rules }
    }

    fn make_rule(entries: &[&str], action: ResolvedAction) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
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
            action: ResolvedAction::Deny,
            message: Some(message.to_string()),
            in_workspace: None,
            in_paths: None,
            source_json: String::new(),
        }
    }

    fn make_workspace_rule(
        entries: &[&str],
        action: ResolvedAction,
        in_workspace: Option<bool>,
    ) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
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
                make_rule(&["Write(**/.env*)"], ResolvedAction::Deny),
                make_rule(&["Write"], ResolvedAction::Allow),
            ],
            DefaultAction::Ask,
        );

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/config/.env.local".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
        assert!(matches!(
            resolve_action_by_name(&config, "Write", &["/src/main.rs".to_string()], None, None),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn unmatched_tool_falls_to_default() {
        let config = make_config(
            vec![make_rule(&["Write"], ResolvedAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(&config, "Read", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn multiple_matchers_in_single_rule() {
        let config = make_config(
            vec![make_rule(&["Read", "Grep", "Glob"], ResolvedAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(&config, "Read", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(&config, "Grep", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(&config, "Write", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn pattern_with_no_input_skips_rule() {
        let config = make_config(
            vec![
                make_rule(&["Write(/src/**)"], ResolvedAction::Allow),
                make_rule(&["Write"], ResolvedAction::Deny),
            ],
            DefaultAction::Ask,
        );
        // No resolved_args: pattern rule can't match, falls through to bare "Write" -> Deny
        assert!(matches!(
            resolve_action_by_name(&config, "Write", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    // --- absolute path enforcement ---

    #[test]
    fn relative_path_without_cwd_is_denied() {
        let config = make_config(
            vec![make_rule(&["Read"], ResolvedAction::Allow)],
            DefaultAction::Ask,
        );
        // Caller passes unresolved relative path (no cwd to resolve against)
        match resolve_action_by_name(
            &config,
            "Read",
            &["relative/path.txt".to_string()],
            None,
            None,
        ) {
            ConfigAction::Decision(ConfigDecision::Deny(Some(msg))) => {
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
            vec![make_rule(&["Read"], ResolvedAction::Allow)],
            DefaultAction::Ask,
        );
        // Caller has already resolved "src/main.rs" against cwd "/home/user/project"
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/project/src/main.rs".to_string()],
                Some("/home/user/project"),
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_shell() {
        let config = make_config(
            vec![make_rule(&["Bash"], ResolvedAction::Allow)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Bash",
                &["ls relative/path".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn absolute_path_check_not_applied_to_unknown_tools() {
        let config = make_config(vec![], DefaultAction::Allow);
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "UnknownTool",
                &["relative/path".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    // --- deny messages ---

    #[test]
    fn deny_rule_with_message() {
        let config = make_config(
            vec![make_deny_rule(&["Delete"], "Use trash instead of delete")],
            DefaultAction::Ask,
        );
        match resolve_action_by_name(&config, "Delete", &[], None, None) {
            ConfigAction::Decision(ConfigDecision::Deny(Some(msg))) => {
                assert_eq!(msg, "Use trash instead of delete");
            }
            other => panic!("expected Deny with message, got {other:?}"),
        }
    }

    #[test]
    fn deny_rule_without_message() {
        let config = make_config(
            vec![make_rule(&["Delete"], ResolvedAction::Deny)],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(&config, "Delete", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Deny(None))
        ));
    }

    #[test]
    fn deny_default_has_no_message() {
        let config = make_config(vec![], DefaultAction::Deny);
        assert!(matches!(
            resolve_action_by_name(&config, "Write", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Deny(None))
        ));
    }

    // --- cwd + pattern matching integration ---

    #[test]
    fn cwd_resolved_path_matches_pattern() {
        let config = make_config(
            vec![
                make_rule(&[r"Read(regex:\.ssh)"], ResolvedAction::Deny),
                make_rule(&["Read"], ResolvedAction::Allow),
            ],
            DefaultAction::Ask,
        );
        // Caller resolved "id_rsa" with cwd ~/.ssh -> /home/user/.ssh/id_rsa
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/.ssh/id_rsa".to_string()],
                Some("/home/user/.ssh"),
                None
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
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
        assert_eq!(expanded, vec!["Bash"]);
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
                    action: ResolvedAction::Deny,
                    message: Some("Access to .ssh is not allowed".into()),
                    in_workspace: None,
                    in_paths: None,
                    source_json: String::new(),
                },
                make_rule(&["Read", "Write"], ResolvedAction::Allow),
            ],
            DefaultAction::Ask,
        );

        match resolve_action_by_name(
            &config,
            "Read",
            &["/home/user/.ssh/id_rsa".to_string()],
            None,
            None,
        ) {
            ConfigAction::Decision(ConfigDecision::Deny(Some(msg))) => {
                assert!(msg.contains(".ssh"))
            }
            other => panic!("expected Deny for .ssh read, got {other:?}"),
        }
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/home/user/.ssh/id_rsa".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/project/main.rs".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    // --- in_workspace rule filter ---

    #[test]
    fn in_workspace_true_allows_workspace_paths() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/project/src/main.rs".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn in_workspace_true_skips_outside_paths() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/etc/passwd".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn in_workspace_false_matches_outside_paths() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Write"], ResolvedAction::Ask, Some(false)),
                make_workspace_rule(&["Write"], ResolvedAction::Allow, None),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/tmp/scratch.txt".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/home/user/project/src/main.rs".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn in_workspace_skipped_for_non_path_tools() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Bash"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // Can't determine workspace membership for Bash, workspace rule skipped, falls to default
        assert!(matches!(
            resolve_action_by_name(&config, "Bash", &["ls".to_string()], None, Some(&roots)),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn in_workspace_skipped_without_roots() {
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );

        // No workspace_roots provided, workspace rule skipped, falls to default
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/project/src/main.rs".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn in_workspace_combined_with_deny_pattern() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Write(**/.git/**)"], ResolvedAction::Deny, Some(true)),
                make_workspace_rule(&["Write"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["Write"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/home/user/project/.git/config".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/home/user/project/src/main.rs".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Write",
                &["/tmp/scratch.txt".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn dotdot_traversal_not_in_workspace() {
        // Verify that ../ traversal out of workspace is correctly detected
        let config = make_config(
            vec![make_workspace_rule(
                &["Read"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // Caller resolved "../.ssh/id_rsa" with cwd /home/user/project -> /home/user/.ssh/id_rsa
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/.ssh/id_rsa".to_string()],
                Some("/home/user/project"),
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    // --- search tools end-to-end ---

    #[test]
    fn grep_outside_workspace_triggers_in_workspace_rule() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Grep",
                &["/home/user/project/src".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Grep",
                &["/home/user/.ssh".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn grep_without_path_or_cwd_skips_workspace_rules() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // No path arg AND no cwd means no workspace determination, both rules skipped
        assert!(matches!(
            resolve_action_by_name(&config, "Grep", &[], None, Some(&roots)),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn grep_without_path_falls_back_to_cwd_for_workspace() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Grep"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["Grep"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // No path arg but cwd is inside workspace => falls back to cwd
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Grep",
                &[],
                Some("/home/user/project"),
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));

        // No path arg but cwd is outside workspace
        assert!(matches!(
            resolve_action_by_name(&config, "Grep", &[], Some("/tmp"), Some(&roots)),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn semantic_search_all_dirs_in_workspace() {
        let config = make_config(
            vec![make_workspace_rule(
                &["SemanticSearch"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "SemanticSearch",
                &[
                    "/home/user/project/src".to_string(),
                    "/home/user/project/lib".to_string()
                ],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn semantic_search_one_dir_outside_workspace_taints() {
        let config = make_config(
            vec![
                make_workspace_rule(&["SemanticSearch"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["SemanticSearch"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // One dir outside workspace => not in_workspace => Ask
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "SemanticSearch",
                &[
                    "/home/user/project/src".to_string(),
                    "/home/user/.ssh".to_string()
                ],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn semantic_search_empty_dirs_skips_workspace_rules() {
        let config = make_config(
            vec![make_workspace_rule(
                &["SemanticSearch"],
                ResolvedAction::Allow,
                Some(true),
            )],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        // Empty dirs = can't determine workspace, rule skipped
        assert!(matches!(
            resolve_action_by_name(&config, "SemanticSearch", &[], None, Some(&roots)),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    // --- in_paths ---

    fn make_in_paths_rule(entries: &[&str], dirs: &[&str], action: ResolvedAction) -> Rule {
        Rule {
            matchers: entries
                .iter()
                .map(|e| parse_tool_entry(e).unwrap())
                .collect(),
            action,
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
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/oss/cilium/main.go".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn in_paths_skips_non_matching_path() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        // Rule skipped, falls to default Ask
        assert!(matches!(
            resolve_action_by_name(&config, "Read", &["/etc/passwd".to_string()], None, None),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn in_paths_no_path_extracted_skips_rule() {
        let config = make_config(
            vec![
                make_in_paths_rule(&["Read"], &["/home/user/oss"], ResolvedAction::Allow),
                make_rule(&["Read"], ResolvedAction::Deny),
            ],
            DefaultAction::Ask,
        );
        // No path field in input — can't check in_paths, rule skipped, falls to next rule
        assert!(matches!(
            resolve_action_by_name(&config, "Read", &[], None, None),
            ConfigAction::Decision(ConfigDecision::Deny(_))
        ));
    }

    #[test]
    fn in_paths_semantic_search_all_in_paths() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["SemanticSearch"],
                &["/home/user/oss"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "SemanticSearch",
                &[
                    "/home/user/oss/cilium/src".to_string(),
                    "/home/user/oss/linux/net".to_string()
                ],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn in_paths_semantic_search_mixed_skips_rule() {
        let config = make_config(
            vec![
                make_in_paths_rule(
                    &["SemanticSearch"],
                    &["/home/user/oss"],
                    ResolvedAction::Allow,
                ),
                make_rule(&["SemanticSearch"], ResolvedAction::Ask),
            ],
            DefaultAction::Deny,
        );
        // One dir inside in_paths, one not — rule skipped, falls to next rule (Ask)
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "SemanticSearch",
                &[
                    "/home/user/oss/cilium/src".to_string(),
                    "/home/user/.ssh".to_string()
                ],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn in_paths_multiple_dirs() {
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss", "/home/user/workspaces"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/oss/cilium/main.go".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/workspaces/project/src/lib.rs".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/.ssh/id_rsa".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn in_paths_composes_with_in_workspace() {
        // Rule requires BOTH in_workspace=true AND in_paths
        let config = make_config(
            vec![Rule {
                matchers: vec![parse_tool_entry("Read").unwrap()],
                action: ResolvedAction::Allow,
                message: None,
                in_workspace: Some(true),
                in_paths: Some(vec!["/home/user/oss".to_string()]),
                source_json: String::new(),
            }],
            DefaultAction::Ask,
        );
        let roots = vec!["/home/user/oss/cilium".to_string()];

        // Inside workspace AND inside in_paths -> Allow
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/oss/cilium/main.go".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));

        // Inside workspace but NOT inside in_paths -> rule skipped -> Ask
        // /home/user/oss/cilium/../other/secret.txt normalises to /home/user/oss/other/secret.txt
        // which IS inside /home/user/oss. Use a path truly outside in_paths:
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/other/secret.txt".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
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
                action: ResolvedAction::Allow,
                message: None,
                in_workspace: None,
                in_paths: Some(vec![expanded.clone()]),
                source_json: String::new(),
            }],
            DefaultAction::Ask,
        );
        let path = format!("{home}/oss/cilium/main.go");
        assert!(matches!(
            resolve_action_by_name(&config, "Read", &[path], None, None),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn in_paths_prefix_not_confused_with_dir() {
        // "/home/user/oss-other" should NOT match in_paths "/home/user/oss"
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/oss-other/main.go".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn in_paths_dotdot_traversal_blocked() {
        // /home/user/oss/../unsafe normalises to /home/user/unsafe — must NOT match in_paths /home/user/oss
        // Caller is responsible for normalizing paths before passing them
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        // Caller has already normalized: /home/user/oss/../unsafe/secret.txt -> /home/user/unsafe/secret.txt
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/unsafe/secret.txt".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }

    #[test]
    fn in_paths_dotdot_within_dir_allowed() {
        // /home/user/oss/cilium/../linux is still inside /home/user/oss — should be allowed
        // Caller has normalized: /home/user/oss/cilium/../linux/net/core.c -> /home/user/oss/linux/net/core.c
        let config = make_config(
            vec![make_in_paths_rule(
                &["Read"],
                &["/home/user/oss"],
                ResolvedAction::Allow,
            )],
            DefaultAction::Ask,
        );
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Read",
                &["/home/user/oss/linux/net/core.c".to_string()],
                None,
                None
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
    }

    #[test]
    fn glob_in_workspace_check() {
        let config = make_config(
            vec![
                make_workspace_rule(&["Glob"], ResolvedAction::Allow, Some(true)),
                make_workspace_rule(&["Glob"], ResolvedAction::Ask, Some(false)),
            ],
            DefaultAction::Deny,
        );
        let roots = vec!["/home/user/project".to_string()];

        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Glob",
                &["/home/user/project/src".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Allow)
        ));
        assert!(matches!(
            resolve_action_by_name(
                &config,
                "Glob",
                &["/home/user/.ssh".to_string()],
                None,
                Some(&roots)
            ),
            ConfigAction::Decision(ConfigDecision::Ask)
        ));
    }
}

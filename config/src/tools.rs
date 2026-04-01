// Tool registry — single source of truth for tool names, categories, and argument extraction.
// Adding a new tool means adding ONE entry here — group aliases, is_path_tool, and argument
// extraction all derive from this table.

#[derive(Clone, Copy, PartialEq)]
pub enum ToolCategory {
    FileRead,
    FileWrite,
    Shell,
    Other,
}

pub struct ToolDef {
    pub name: &'static str,
    pub category: ToolCategory,
    /// JSON field names to try in order for scalar arg extraction.
    pub fields: &'static [&'static str],
    /// If set, extract from this JSON array field instead of scalar fields.
    pub array_field: Option<&'static str>,
}

pub const TOOL_DEFS: &[ToolDef] = &[
    // File read
    ToolDef {
        name: "Read",
        category: ToolCategory::FileRead,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "Grep",
        category: ToolCategory::FileRead,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "Glob",
        category: ToolCategory::FileRead,
        fields: &["target_directory"],
        array_field: None,
    },
    ToolDef {
        name: "SemanticSearch",
        category: ToolCategory::FileRead,
        fields: &[],
        array_field: Some("target_directories"),
    },
    // File write
    ToolDef {
        name: "Write",
        category: ToolCategory::FileWrite,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "StrReplace",
        category: ToolCategory::FileWrite,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "Delete",
        category: ToolCategory::FileWrite,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "Edit",
        category: ToolCategory::FileWrite,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "MultiEdit",
        category: ToolCategory::FileWrite,
        fields: &["path", "file_path"],
        array_field: None,
    },
    ToolDef {
        name: "EditNotebook",
        category: ToolCategory::FileWrite,
        fields: &["target_notebook"],
        array_field: None,
    },
    ToolDef {
        name: "NotebookEdit",
        category: ToolCategory::FileWrite,
        fields: &["target_notebook"],
        array_field: None,
    },
    // Shell
    ToolDef {
        name: "Bash",
        category: ToolCategory::Shell,
        fields: &["command"],
        array_field: None,
    },
    ToolDef {
        name: "Shell",
        category: ToolCategory::Shell,
        fields: &["command"],
        array_field: None,
    },
    // Other (has arg extraction but not in a file/shell group)
    ToolDef {
        name: "WebFetch",
        category: ToolCategory::Other,
        fields: &["url"],
        array_field: None,
    },
];

pub fn find_tool_def(name: &str) -> Option<&'static ToolDef> {
    TOOL_DEFS.iter().find(|d| d.name == name)
}

pub fn tools_in_category(cat: ToolCategory) -> Vec<&'static str> {
    TOOL_DEFS
        .iter()
        .filter(|d| d.category == cat)
        .map(|d| d.name)
        .collect()
}

pub fn is_path_tool(tool_name: &str) -> bool {
    find_tool_def(tool_name)
        .is_some_and(|d| matches!(d.category, ToolCategory::FileRead | ToolCategory::FileWrite))
}

pub fn expand_tool_group(name: &str) -> Option<Vec<&'static str>> {
    match name {
        "@file" => {
            let mut v = tools_in_category(ToolCategory::FileRead);
            v.extend(tools_in_category(ToolCategory::FileWrite));
            Some(v)
        }
        "@file_read" => Some(tools_in_category(ToolCategory::FileRead)),
        "@file_write" => Some(tools_in_category(ToolCategory::FileWrite)),
        "@shell" => Some(tools_in_category(ToolCategory::Shell)),
        _ => None,
    }
}

pub fn is_in_workspace(path: &str, workspace_roots: &[String]) -> bool {
    workspace_roots.iter().any(|root| {
        let root = root.trim_end_matches('/');
        path == root || path.starts_with(&format!("{root}/"))
    })
}

/// Map a provider-native tool name to its canonical name.
/// For providers that use Claude Code tool names natively, the map is empty and names pass through.
pub fn normalise_tool_name<'a>(name: &'a str, map: &[(&'static str, &'static str)]) -> &'a str {
    map.iter()
        .find(|(native, _)| *native == name)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(name)
}

/// Normalize a path by resolving `.` and `..` segments without filesystem access.
pub fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if matches!(components.last(), Some(std::path::Component::Normal(_))) {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

pub fn resolve_path(raw: &str, tool_name: &str, cwd: Option<&str>) -> String {
    if is_path_tool(tool_name) {
        if let Some(cwd) = cwd {
            let path = std::path::Path::new(raw);
            if path.is_relative() {
                let joined = std::path::Path::new(cwd).join(path);
                return normalize_path(&joined).to_string_lossy().into_owned();
            }
        }
        return normalize_path(std::path::Path::new(raw))
            .to_string_lossy()
            .into_owned();
    }
    raw.to_string()
}

/// Extract the arguments to match against from tool_input, based on tool type.
/// For path-based tools, relative paths are resolved against cwd.
/// Returns multiple values for tools like SemanticSearch that accept an array of paths.
pub fn get_matchable_args(
    tool_name: &str,
    tool_input: &serde_json::Value,
    cwd: Option<&str>,
) -> Vec<String> {
    let def = match find_tool_def(tool_name) {
        Some(d) => d,
        None => return vec![],
    };

    if let Some(array_field) = def.array_field {
        tool_input
            .get(array_field)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| resolve_path(s, tool_name, cwd))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        let raw = def
            .fields
            .iter()
            .find_map(|field| tool_input.get(*field).and_then(|v| v.as_str()));
        match raw {
            Some(s) => vec![resolve_path(s, tool_name, cwd)],
            None => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- get_matchable_args ---

    #[test]
    fn matchable_arg_file_tools() {
        let input = serde_json::json!({"path": "/foo/bar"});
        assert_eq!(get_matchable_args("Read", &input, None), vec!["/foo/bar"]);
        assert_eq!(get_matchable_args("Write", &input, None), vec!["/foo/bar"]);
        assert_eq!(
            get_matchable_args("StrReplace", &input, None),
            vec!["/foo/bar"]
        );
        assert_eq!(get_matchable_args("Delete", &input, None), vec!["/foo/bar"]);
    }

    #[test]
    fn matchable_arg_file_path_fallback() {
        let input = serde_json::json!({"file_path": "/foo/bar"});
        assert_eq!(get_matchable_args("Edit", &input, None), vec!["/foo/bar"]);
        assert_eq!(get_matchable_args("Read", &input, None), vec!["/foo/bar"]);
    }

    #[test]
    fn matchable_arg_shell() {
        let input = serde_json::json!({"command": "ls -la"});
        assert_eq!(get_matchable_args("Bash", &input, None), vec!["ls -la"]);
        assert_eq!(get_matchable_args("Shell", &input, None), vec!["ls -la"]);
    }

    #[test]
    fn matchable_arg_notebook() {
        let input = serde_json::json!({"target_notebook": "analysis.ipynb"});
        assert_eq!(
            get_matchable_args("EditNotebook", &input, None),
            vec!["analysis.ipynb"]
        );
    }

    #[test]
    fn matchable_arg_unknown_tool() {
        let input = serde_json::json!({"whatever": "value"});
        assert!(get_matchable_args("UnknownTool", &input, None).is_empty());
    }

    #[test]
    fn matchable_arg_grep() {
        let input = serde_json::json!({"pattern": "TODO", "path": "/home/user/project"});
        assert_eq!(
            get_matchable_args("Grep", &input, None),
            vec!["/home/user/project"]
        );
    }

    #[test]
    fn matchable_arg_grep_no_path() {
        let input = serde_json::json!({"pattern": "TODO"});
        assert!(get_matchable_args("Grep", &input, None).is_empty());
    }

    #[test]
    fn matchable_arg_glob() {
        let input =
            serde_json::json!({"glob_pattern": "*.rs", "target_directory": "/home/user/project"});
        assert_eq!(
            get_matchable_args("Glob", &input, None),
            vec!["/home/user/project"]
        );
    }

    #[test]
    fn matchable_arg_semantic_search() {
        let input = serde_json::json!({
            "query": "auth flow",
            "target_directories": ["/home/user/project", "/home/user/lib"]
        });
        assert_eq!(
            get_matchable_args("SemanticSearch", &input, None),
            vec!["/home/user/project", "/home/user/lib"]
        );
    }

    #[test]
    fn matchable_arg_semantic_search_empty_array() {
        let input = serde_json::json!({"query": "auth flow", "target_directories": []});
        assert!(get_matchable_args("SemanticSearch", &input, None).is_empty());
    }

    #[test]
    fn matchable_arg_semantic_search_filters_empty_strings() {
        let input =
            serde_json::json!({"query": "auth", "target_directories": ["", "/home/user/project"]});
        assert_eq!(
            get_matchable_args("SemanticSearch", &input, None),
            vec!["/home/user/project"]
        );
    }

    // --- cwd resolution ---

    #[test]
    fn cwd_resolves_relative_path() {
        let input = serde_json::json!({"path": "id_rsa"});
        assert_eq!(
            get_matchable_args("Read", &input, Some("/home/user/.ssh")),
            vec!["/home/user/.ssh/id_rsa"]
        );
    }

    #[test]
    fn cwd_leaves_absolute_path_unchanged() {
        let input = serde_json::json!({"path": "/etc/passwd"});
        assert_eq!(
            get_matchable_args("Read", &input, Some("/home/user")),
            vec!["/etc/passwd"]
        );
    }

    #[test]
    fn cwd_not_applied_to_shell_commands() {
        let input = serde_json::json!({"command": "cat id_rsa"});
        assert_eq!(
            get_matchable_args("Bash", &input, Some("/home/user/.ssh")),
            vec!["cat id_rsa"]
        );
    }

    #[test]
    fn cwd_not_applied_to_urls() {
        let input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(
            get_matchable_args("WebFetch", &input, Some("/home/user")),
            vec!["https://example.com"]
        );
    }

    #[test]
    fn cwd_resolves_dotdot_in_relative_path() {
        let input = serde_json::json!({"path": "../.ssh/id_rsa"});
        let resolved = get_matchable_args("Read", &input, Some("/home/user/project"));
        assert_eq!(resolved, vec!["/home/user/.ssh/id_rsa"]);
    }

    #[test]
    fn normalize_removes_dot_segments() {
        let input = serde_json::json!({"path": "/home/user/./project/./main.rs"});
        assert_eq!(
            get_matchable_args("Read", &input, None),
            vec!["/home/user/project/main.rs"]
        );
    }

    #[test]
    fn normalize_absolute_path_with_dotdot() {
        let input = serde_json::json!({"path": "/home/user/project/../.ssh/id_rsa"});
        assert_eq!(
            get_matchable_args("Read", &input, None),
            vec!["/home/user/.ssh/id_rsa"]
        );
    }

    #[test]
    fn cwd_resolves_relative_grep_path() {
        let input = serde_json::json!({"pattern": "secret", "path": "subdir"});
        assert_eq!(
            get_matchable_args("Grep", &input, Some("/home/user/project")),
            vec!["/home/user/project/subdir"]
        );
    }

    #[test]
    fn cwd_resolves_semantic_search_relative_dirs() {
        let input = serde_json::json!({
            "query": "auth",
            "target_directories": ["src/", "/absolute/path"]
        });
        let resolved = get_matchable_args("SemanticSearch", &input, Some("/home/user/project"));
        assert_eq!(resolved, vec!["/home/user/project/src", "/absolute/path"]);
    }

    // --- is_in_workspace ---

    #[test]
    fn path_inside_workspace() {
        let roots = vec!["/home/user/project".to_string()];
        assert!(is_in_workspace("/home/user/project/src/main.rs", &roots));
        assert!(is_in_workspace("/home/user/project", &roots));
    }

    #[test]
    fn path_outside_workspace() {
        let roots = vec!["/home/user/project".to_string()];
        assert!(!is_in_workspace("/home/user/.ssh/id_rsa", &roots));
        assert!(!is_in_workspace("/etc/passwd", &roots));
    }

    #[test]
    fn path_prefix_not_confused_with_workspace() {
        let roots = vec!["/home/user/project".to_string()];
        // "/home/user/project-other/foo" starts with "/home/user/project"
        // but is NOT inside the workspace
        assert!(!is_in_workspace("/home/user/project-other/foo", &roots));
    }

    #[test]
    fn multiple_workspace_roots() {
        let roots = vec![
            "/home/user/project-a".to_string(),
            "/home/user/project-b".to_string(),
        ];
        assert!(is_in_workspace("/home/user/project-a/src/main.rs", &roots));
        assert!(is_in_workspace("/home/user/project-b/lib.rs", &roots));
        assert!(!is_in_workspace("/home/user/other/file.rs", &roots));
    }

    #[test]
    fn workspace_root_with_trailing_slash() {
        let roots = vec!["/home/user/project/".to_string()];
        assert!(is_in_workspace("/home/user/project/src/main.rs", &roots));
    }
}

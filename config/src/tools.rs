// Tool helpers for the config rule engine.
//
// Category, group expansion, and `is_path_tool` now live on `protocol::Tool`.
// This module keeps the path-resolution and workspace-membership helpers that
// the rule engine needs.

use protocol::Tool;

pub fn is_in_workspace(path: &str, workspace_roots: &[String]) -> bool {
    workspace_roots.iter().any(|root| {
        let root = root.trim_end_matches('/');
        path == root || path.starts_with(&format!("{root}/"))
    })
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

pub fn resolve_path(raw: &str, tool: &Tool, cwd: Option<&str>) -> String {
    if tool.is_path_tool() {
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- cwd resolution ---

    #[test]
    fn cwd_resolves_relative_path() {
        assert_eq!(
            resolve_path("id_rsa", &Tool::Read, Some("/home/user/.ssh")),
            "/home/user/.ssh/id_rsa"
        );
    }

    #[test]
    fn cwd_leaves_absolute_path_unchanged() {
        assert_eq!(
            resolve_path("/etc/passwd", &Tool::Read, Some("/home/user")),
            "/etc/passwd"
        );
    }

    #[test]
    fn cwd_not_applied_to_shell_commands() {
        assert_eq!(
            resolve_path("cat id_rsa", &Tool::Bash, Some("/home/user/.ssh")),
            "cat id_rsa"
        );
    }

    #[test]
    fn cwd_not_applied_to_urls() {
        assert_eq!(
            resolve_path("https://example.com", &Tool::WebFetch, Some("/home/user")),
            "https://example.com"
        );
    }

    #[test]
    fn cwd_resolves_dotdot_in_relative_path() {
        let resolved = resolve_path("../.ssh/id_rsa", &Tool::Read, Some("/home/user/project"));
        assert_eq!(resolved, "/home/user/.ssh/id_rsa");
    }

    #[test]
    fn normalize_removes_dot_segments() {
        assert_eq!(
            resolve_path("/home/user/./project/./main.rs", &Tool::Read, None),
            "/home/user/project/main.rs"
        );
    }

    #[test]
    fn normalize_absolute_path_with_dotdot() {
        assert_eq!(
            resolve_path("/home/user/project/../.ssh/id_rsa", &Tool::Read, None),
            "/home/user/.ssh/id_rsa"
        );
    }

    #[test]
    fn cwd_resolves_relative_grep_path() {
        assert_eq!(
            resolve_path("subdir", &Tool::Grep, Some("/home/user/project")),
            "/home/user/project/subdir"
        );
    }

    #[test]
    fn cwd_not_applied_to_unknown_tools() {
        let tool = Tool::Unknown("MyCoolTool".to_owned());
        assert_eq!(
            resolve_path("relative/path", &tool, Some("/home/user")),
            "relative/path"
        );
    }

    #[test]
    fn cwd_not_applied_to_meta_tools() {
        assert_eq!(
            resolve_path("something", &Tool::Task, Some("/home/user")),
            "something"
        );
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

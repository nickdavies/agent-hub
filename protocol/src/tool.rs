//! Canonical tool identity used across the system.
//!
//! `Tool` is the normalized representation of a tool name. It serializes to/from
//! the Claude Code PascalCase name (the canonical format). Unknown tools serialize
//! as their raw string — the system passes them through without special handling.
//!
//! Category classification, group expansion, and `is_path_tool` all live here so
//! that adding a new `Tool` variant forces you to assign its category (Rust's
//! exhaustive match guarantees this at compile time).

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use strum::{Display, EnumString};

// ===========================================================================
// ToolCategory — broad classification for rule-engine behaviour
// ===========================================================================

/// Broad category that determines how the config rule engine treats a tool.
///
/// `FileRead` and `FileWrite` are "path tools" — the rule engine resolves
/// relative paths and enforces workspace/`in_paths` constraints on them.
/// `Shell` and `Other` are non-path tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    FileRead,
    FileWrite,
    Shell,
    Other,
}

// ===========================================================================
// ToolGroup — named aliases for config rules
// ===========================================================================

/// Named groups that users can reference in config rules with an `@` prefix
/// (e.g. `@file`, `@file_read`, `@shell`).
#[derive(Debug, Clone, Copy, PartialEq, EnumString, Display)]
pub enum ToolGroup {
    #[strum(serialize = "@file")]
    File,
    #[strum(serialize = "@file_read")]
    FileRead,
    #[strum(serialize = "@file_write")]
    FileWrite,
    #[strum(serialize = "@shell")]
    Shell,
}

impl ToolGroup {
    /// Expand the group into its constituent tool names.
    pub fn expand(self) -> Vec<&'static str> {
        match self {
            Self::File => {
                let mut v = Tool::tools_in_category(ToolCategory::FileRead).to_vec();
                v.extend_from_slice(Tool::tools_in_category(ToolCategory::FileWrite));
                v
            }
            Self::FileRead => Tool::tools_in_category(ToolCategory::FileRead).to_vec(),
            Self::FileWrite => Tool::tools_in_category(ToolCategory::FileWrite).to_vec(),
            Self::Shell => Tool::tools_in_category(ToolCategory::Shell).to_vec(),
        }
    }
}

/// Expand an `@`-prefixed group name into its constituent tool names.
/// Returns `None` if the name is not a recognised group.
pub fn expand_tool_group(name: &str) -> Option<Vec<&'static str>> {
    name.parse::<ToolGroup>().ok().map(|g| g.expand())
}

// ===========================================================================
// Tool — canonical identity enum
// ===========================================================================

/// Canonical tool identity used across the system.
///
/// Known variants map 1:1 to Claude Code's PascalCase tool names.
/// `Unknown(String)` captures MCP tools, future tools, or anything not yet
/// in the enum — they round-trip through serde as their raw string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Tool {
    // File read
    Read,
    Grep,
    Glob,
    SemanticSearch,

    // File write
    Write,
    StrReplace,
    Delete,
    Edit,
    MultiEdit,
    EditNotebook,
    NotebookEdit,

    // Shell
    Bash,

    // Agent / meta
    Task,
    TodoWrite,

    // Other
    WebFetch,

    // Extensibility — MCP tools, future tools, etc.
    Unknown(String),
}

impl Tool {
    /// The canonical string name (Claude Code PascalCase format).
    pub fn as_str(&self) -> &str {
        match self {
            Tool::Read => "Read",
            Tool::Grep => "Grep",
            Tool::Glob => "Glob",
            Tool::SemanticSearch => "SemanticSearch",
            Tool::Write => "Write",
            Tool::StrReplace => "StrReplace",
            Tool::Delete => "Delete",
            Tool::Edit => "Edit",
            Tool::MultiEdit => "MultiEdit",
            Tool::EditNotebook => "EditNotebook",
            Tool::NotebookEdit => "NotebookEdit",
            Tool::Bash => "Bash",
            Tool::Task => "Task",
            Tool::TodoWrite => "TodoWrite",
            Tool::WebFetch => "WebFetch",
            Tool::Unknown(s) => s.as_str(),
        }
    }

    /// The broad category for this tool, or `None` for unknown/MCP tools.
    ///
    /// Adding a new `Tool` variant without a `category()` arm is a compile error,
    /// which is the key advantage over the old `TOOL_DEFS` table.
    pub fn category(&self) -> Option<ToolCategory> {
        match self {
            Tool::Read | Tool::Grep | Tool::Glob | Tool::SemanticSearch => {
                Some(ToolCategory::FileRead)
            }
            Tool::Write
            | Tool::StrReplace
            | Tool::Delete
            | Tool::Edit
            | Tool::MultiEdit
            | Tool::EditNotebook
            | Tool::NotebookEdit => Some(ToolCategory::FileWrite),
            Tool::Bash => Some(ToolCategory::Shell),
            Tool::WebFetch => Some(ToolCategory::Other),
            // Agent/meta tools and unknown tools have no category.
            Tool::Task | Tool::TodoWrite | Tool::Unknown(_) => None,
        }
    }

    /// Whether this tool operates on file paths and should have path resolution
    /// and workspace/`in_paths` enforcement applied by the rule engine.
    pub fn is_path_tool(&self) -> bool {
        matches!(
            self.category(),
            Some(ToolCategory::FileRead | ToolCategory::FileWrite)
        )
    }

    /// All known tool names in the given category.
    ///
    /// This is a hardcoded list per category. The `tools_in_category_match_category`
    /// test verifies that every name listed here actually has the claimed category,
    /// and that no known tool is missing from its category's list.
    pub fn tools_in_category(cat: ToolCategory) -> &'static [&'static str] {
        match cat {
            ToolCategory::FileRead => &["Read", "Grep", "Glob", "SemanticSearch"],
            ToolCategory::FileWrite => &[
                "Write",
                "StrReplace",
                "Delete",
                "Edit",
                "MultiEdit",
                "EditNotebook",
                "NotebookEdit",
            ],
            ToolCategory::Shell => &["Bash"],
            ToolCategory::Other => &["WebFetch"],
        }
    }

    /// Parse a canonical string into a `Tool`.
    fn from_canonical(s: &str) -> Tool {
        match s {
            "Read" => Tool::Read,
            "Grep" => Tool::Grep,
            "Glob" => Tool::Glob,
            "SemanticSearch" => Tool::SemanticSearch,
            "Write" => Tool::Write,
            "StrReplace" => Tool::StrReplace,
            "Delete" => Tool::Delete,
            "Edit" => Tool::Edit,
            "MultiEdit" => Tool::MultiEdit,
            "EditNotebook" => Tool::EditNotebook,
            "NotebookEdit" => Tool::NotebookEdit,
            "Bash" => Tool::Bash,
            "Task" => Tool::Task,
            "TodoWrite" => Tool::TodoWrite,
            "WebFetch" => Tool::WebFetch,
            other => Tool::Unknown(other.to_owned()),
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Tool {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// --- Serde ---

impl Serialize for Tool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Tool::from_canonical(&s))
    }
}

// --- JsonSchema ---
//
// schemars 1.x doesn't derive cleanly for untagged-style enums with data variants,
// so we implement manually. The schema says "any string" with known values documented.

impl JsonSchema for Tool {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Tool".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Canonical tool name. Known values: Read, Grep, Glob, SemanticSearch, Write, StrReplace, Delete, Edit, MultiEdit, EditNotebook, NotebookEdit, Bash, Task, TodoWrite, WebFetch. Arbitrary strings are accepted for unknown/MCP tools."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tool_roundtrip() {
        let tool = Tool::Bash;
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, r#""Bash""#);
        let back: Tool = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tool::Bash);
    }

    #[test]
    fn unknown_tool_roundtrip() {
        let tool = Tool::Unknown("my_mcp_tool".to_owned());
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, r#""my_mcp_tool""#);
        let back: Tool = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tool::Unknown("my_mcp_tool".to_owned()));
    }

    #[test]
    fn display_known() {
        assert_eq!(Tool::SemanticSearch.to_string(), "SemanticSearch");
    }

    #[test]
    fn display_unknown() {
        assert_eq!(Tool::Unknown("custom".to_owned()).to_string(), "custom");
    }

    #[test]
    fn as_str_matches_display() {
        for tool in [
            Tool::Read,
            Tool::Grep,
            Tool::Glob,
            Tool::Write,
            Tool::Bash,
            Tool::Task,
            Tool::TodoWrite,
            Tool::Unknown("x".to_owned()),
        ] {
            assert_eq!(tool.as_str(), tool.to_string());
        }
    }

    #[test]
    fn all_known_variants_deserialize() {
        let names = [
            "Read",
            "Grep",
            "Glob",
            "SemanticSearch",
            "Write",
            "StrReplace",
            "Delete",
            "Edit",
            "MultiEdit",
            "EditNotebook",
            "NotebookEdit",
            "Bash",
            "Task",
            "TodoWrite",
            "WebFetch",
        ];
        for name in names {
            let json = format!("\"{name}\"");
            let tool: Tool = serde_json::from_str(&json).unwrap();
            assert!(
                !matches!(tool, Tool::Unknown(_)),
                "{name} deserialized as Unknown"
            );
            assert_eq!(tool.as_str(), name);
        }
    }

    // --- category / is_path_tool ---

    #[test]
    fn file_read_tools_have_correct_category() {
        for tool in [Tool::Read, Tool::Grep, Tool::Glob, Tool::SemanticSearch] {
            assert_eq!(
                tool.category(),
                Some(ToolCategory::FileRead),
                "{tool} should be FileRead"
            );
            assert!(tool.is_path_tool(), "{tool} should be a path tool");
        }
    }

    #[test]
    fn file_write_tools_have_correct_category() {
        for tool in [
            Tool::Write,
            Tool::StrReplace,
            Tool::Delete,
            Tool::Edit,
            Tool::MultiEdit,
            Tool::EditNotebook,
            Tool::NotebookEdit,
        ] {
            assert_eq!(
                tool.category(),
                Some(ToolCategory::FileWrite),
                "{tool} should be FileWrite"
            );
            assert!(tool.is_path_tool(), "{tool} should be a path tool");
        }
    }

    #[test]
    fn shell_tools_have_correct_category() {
        assert_eq!(Tool::Bash.category(), Some(ToolCategory::Shell));
        assert!(!Tool::Bash.is_path_tool());
    }

    #[test]
    fn other_tools_have_correct_category() {
        assert_eq!(Tool::WebFetch.category(), Some(ToolCategory::Other));
        assert!(!Tool::WebFetch.is_path_tool());
    }

    #[test]
    fn meta_and_unknown_tools_have_no_category() {
        assert_eq!(Tool::Task.category(), None);
        assert_eq!(Tool::TodoWrite.category(), None);
        assert_eq!(Tool::Unknown("mcp".to_owned()).category(), None);
        assert!(!Tool::Task.is_path_tool());
        assert!(!Tool::TodoWrite.is_path_tool());
        assert!(!Tool::Unknown("mcp".to_owned()).is_path_tool());
    }

    /// Verify that `tools_in_category` is consistent with `category()`:
    /// every name listed actually has that category, and every known tool
    /// with a category appears in its list.
    #[test]
    fn tools_in_category_matches_category() {
        let all_categories = [
            ToolCategory::FileRead,
            ToolCategory::FileWrite,
            ToolCategory::Shell,
            ToolCategory::Other,
        ];
        // Every name in the list deserializes to a tool with the right category.
        for cat in &all_categories {
            for name in Tool::tools_in_category(*cat) {
                let tool = Tool::from_canonical(name);
                assert_eq!(
                    tool.category(),
                    Some(*cat),
                    "{name} is listed under {cat:?} but category() disagrees"
                );
            }
        }
        // Every known variant that has a category appears in the right list.
        let all_known = [
            "Read",
            "Grep",
            "Glob",
            "SemanticSearch",
            "Write",
            "StrReplace",
            "Delete",
            "Edit",
            "MultiEdit",
            "EditNotebook",
            "NotebookEdit",
            "Bash",
            "Task",
            "TodoWrite",
            "WebFetch",
        ];
        for name in all_known {
            let tool = Tool::from_canonical(name);
            if let Some(cat) = tool.category() {
                assert!(
                    Tool::tools_in_category(cat).contains(&name),
                    "{name} has category {cat:?} but is missing from tools_in_category"
                );
            }
        }
    }

    // --- tools_in_category ---

    #[test]
    fn tools_in_category_file_read() {
        let tools = Tool::tools_in_category(ToolCategory::FileRead);
        assert!(tools.contains(&"Read"));
        assert!(tools.contains(&"Grep"));
        assert!(tools.contains(&"Glob"));
        assert!(tools.contains(&"SemanticSearch"));
        assert!(!tools.contains(&"Write"));
        assert!(!tools.contains(&"Bash"));
    }

    #[test]
    fn tools_in_category_file_write() {
        let tools = Tool::tools_in_category(ToolCategory::FileWrite);
        assert!(tools.contains(&"Write"));
        assert!(tools.contains(&"StrReplace"));
        assert!(tools.contains(&"Delete"));
        assert!(tools.contains(&"Edit"));
        assert!(tools.contains(&"MultiEdit"));
        assert!(tools.contains(&"EditNotebook"));
        assert!(tools.contains(&"NotebookEdit"));
        assert!(!tools.contains(&"Read"));
    }

    #[test]
    fn tools_in_category_shell() {
        assert_eq!(Tool::tools_in_category(ToolCategory::Shell), &["Bash"]);
    }

    // --- ToolGroup ---

    #[test]
    fn tool_group_expand_file() {
        let tools = ToolGroup::File.expand();
        let read_tools = Tool::tools_in_category(ToolCategory::FileRead);
        let write_tools = Tool::tools_in_category(ToolCategory::FileWrite);
        assert_eq!(tools.len(), read_tools.len() + write_tools.len());
        assert!(tools.contains(&"Read"));
        assert!(tools.contains(&"Write"));
    }

    #[test]
    fn tool_group_expand_shell() {
        assert_eq!(ToolGroup::Shell.expand(), vec!["Bash"]);
    }

    #[test]
    fn expand_tool_group_valid() {
        assert!(expand_tool_group("@file").is_some());
        assert!(expand_tool_group("@file_read").is_some());
        assert!(expand_tool_group("@file_write").is_some());
        assert!(expand_tool_group("@shell").is_some());
    }

    #[test]
    fn expand_tool_group_invalid() {
        assert!(expand_tool_group("@Shell").is_none());
        assert!(expand_tool_group("@files_write").is_none());
        assert!(expand_tool_group("file").is_none());
    }
}

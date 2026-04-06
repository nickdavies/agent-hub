//! Typed tool call — merges tool identity with typed arguments.
//!
//! `ToolCall` bundles the typed variant ([`ToolCallKind`]) with the original raw
//! JSON blob from the provider. All gateway logic works through the typed fields;
//! the raw blob is carried for display, passthrough, and wire-format reconstruction.
//!
//! Construction is via `TryFrom<(Tool, serde_json::Value)>` — the provider-specific
//! `TryFrom` impls produce a `(Tool, Value)` pair, and this module handles the
//! typed deserialization.

use serde::Deserialize;
use serde_json::Value;
use std::fmt;

use crate::Tool;

// ===========================================================================
// ToolCallKind — the typed enum
// ===========================================================================

/// Typed representation of a tool call's identity and arguments.
///
/// Each known variant carries exactly the fields the gateway logic needs.
/// Variants with empty bodies (`EditNotebook`, `NotebookEdit`, `Task`, `TodoWrite`)
/// are tools where we don't currently extract fields — the strictness goal applies
/// to every field we DO use, not to exhaustive coverage.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallKind {
    // File read
    Read {
        path: String,
    },
    Grep {
        path: Option<String>,
        pattern: String,
    },
    Glob {
        target_directory: String,
    },
    SemanticSearch {
        target_directories: Vec<String>,
    },

    // File write
    Write {
        path: String,
        content: Option<String>,
    },
    StrReplace {
        path: Option<String>,
        old_string: String,
        new_string: String,
    },
    Delete {
        path: String,
    },
    Edit {
        path: Option<String>,
        old_content: String,
        new_content: String,
    },
    MultiEdit {
        path: Option<String>,
        edits: Vec<MultiEditEntry>,
    },
    EditNotebook {},
    NotebookEdit {},

    // Shell
    Bash {
        command: String,
    },

    // Agent / meta
    Task {},
    TodoWrite {},

    // Other
    WebFetch {
        url: String,
    },

    // Extensibility — typed args unavailable; raw blob in ToolCall.raw only
    Unknown {
        name: String,
    },
}

/// A single edit within a [`ToolCallKind::MultiEdit`] operation.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiEditEntry {
    pub old_string: String,
    pub new_string: String,
}

// ===========================================================================
// ToolCall — the wrapper struct
// ===========================================================================

/// A tool call with both typed fields and the original raw JSON blob.
///
/// - **`kind`** — the typed enum; all gateway logic reads from this.
/// - **`raw`** — the original `serde_json::Value` from the provider; kept for
///   display, passthrough to server/delegate, and wire-format reconstruction.
///   No code should ever introspect `raw` for logic decisions.
#[derive(Debug, Clone)]
pub struct ToolCall {
    kind: ToolCallKind,
    raw: Value,
}

impl ToolCall {
    /// The canonical tool identity derived from the typed variant.
    pub fn tool(&self) -> Tool {
        match &self.kind {
            ToolCallKind::Read { .. } => Tool::Read,
            ToolCallKind::Grep { .. } => Tool::Grep,
            ToolCallKind::Glob { .. } => Tool::Glob,
            ToolCallKind::SemanticSearch { .. } => Tool::SemanticSearch,
            ToolCallKind::Write { .. } => Tool::Write,
            ToolCallKind::StrReplace { .. } => Tool::StrReplace,
            ToolCallKind::Delete { .. } => Tool::Delete,
            ToolCallKind::Edit { .. } => Tool::Edit,
            ToolCallKind::MultiEdit { .. } => Tool::MultiEdit,
            ToolCallKind::EditNotebook {} => Tool::EditNotebook,
            ToolCallKind::NotebookEdit {} => Tool::NotebookEdit,
            ToolCallKind::Bash { .. } => Tool::Bash,
            ToolCallKind::Task {} => Tool::Task,
            ToolCallKind::TodoWrite {} => Tool::TodoWrite,
            ToolCallKind::WebFetch { .. } => Tool::WebFetch,
            ToolCallKind::Unknown { name } => Tool::Unknown(name.clone()),
        }
    }

    /// The canonical tool name as a string slice.
    ///
    /// For known tools this is a `&'static str` (e.g. `"Read"`, `"Bash"`).
    /// For unknown tools it borrows from the stored name.
    pub fn tool_name(&self) -> &str {
        match &self.kind {
            ToolCallKind::Read { .. } => "Read",
            ToolCallKind::Grep { .. } => "Grep",
            ToolCallKind::Glob { .. } => "Glob",
            ToolCallKind::SemanticSearch { .. } => "SemanticSearch",
            ToolCallKind::Write { .. } => "Write",
            ToolCallKind::StrReplace { .. } => "StrReplace",
            ToolCallKind::Delete { .. } => "Delete",
            ToolCallKind::Edit { .. } => "Edit",
            ToolCallKind::MultiEdit { .. } => "MultiEdit",
            ToolCallKind::EditNotebook {} => "EditNotebook",
            ToolCallKind::NotebookEdit {} => "NotebookEdit",
            ToolCallKind::Bash { .. } => "Bash",
            ToolCallKind::Task {} => "Task",
            ToolCallKind::TodoWrite {} => "TodoWrite",
            ToolCallKind::WebFetch { .. } => "WebFetch",
            ToolCallKind::Unknown { name } => name.as_str(),
        }
    }

    /// The original raw JSON blob from the provider (display/passthrough only).
    pub fn raw_input(&self) -> &Value {
        &self.raw
    }

    /// The typed variant — all gateway logic should branch on this.
    pub fn kind(&self) -> &ToolCallKind {
        &self.kind
    }

    /// Arguments suitable for config-engine pattern matching.
    ///
    /// Returns the path/command/url/directory args that the rule engine matches
    /// against. For path-tools these are the file paths; for Bash it's the command;
    /// for SemanticSearch it's each target directory.
    ///
    /// The caller is responsible for resolving relative paths (via `config::resolve_path`)
    /// before passing these to `resolve_action`.
    ///
    /// Returns an empty vec for tools with no matchable arguments (Task, TodoWrite,
    /// EditNotebook, NotebookEdit, Unknown).
    pub fn matchable_args(&self) -> Vec<&str> {
        match &self.kind {
            // File read — single path arg (Grep path is optional)
            ToolCallKind::Read { path } => vec![path.as_str()],
            ToolCallKind::Grep { path, .. } => path.as_deref().into_iter().collect(),
            ToolCallKind::Glob { target_directory } => vec![target_directory.as_str()],
            ToolCallKind::SemanticSearch { target_directories } => {
                target_directories.iter().map(String::as_str).collect()
            }

            // File write — single path arg (some are optional)
            ToolCallKind::Write { path, .. } => vec![path.as_str()],
            ToolCallKind::StrReplace { path, .. } => path.as_deref().into_iter().collect(),
            ToolCallKind::Delete { path } => vec![path.as_str()],
            ToolCallKind::Edit { path, .. } => path.as_deref().into_iter().collect(),
            ToolCallKind::MultiEdit { path, .. } => path.as_deref().into_iter().collect(),

            // Shell
            ToolCallKind::Bash { command } => vec![command.as_str()],

            // Other
            ToolCallKind::WebFetch { url } => vec![url.as_str()],

            // No matchable args
            ToolCallKind::EditNotebook {}
            | ToolCallKind::NotebookEdit {}
            | ToolCallKind::Task {}
            | ToolCallKind::TodoWrite {}
            | ToolCallKind::Unknown { .. } => vec![],
        }
    }
}

// ===========================================================================
// Construction error
// ===========================================================================

/// Error returned when a `(Tool, Value)` pair cannot be parsed into a `ToolCall`.
///
/// This typically means the provider sent a known tool name with malformed arguments
/// (e.g. Write without a `path` field).
#[derive(Debug)]
pub struct ToolCallParseError {
    pub tool: String,
    pub message: String,
}

impl fmt::Display for ToolCallParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to parse {} tool call: {}",
            self.tool, self.message
        )
    }
}

impl std::error::Error for ToolCallParseError {}

// ===========================================================================
// TryFrom<(Tool, Value)> — the single construction path
// ===========================================================================

impl TryFrom<(Tool, Value)> for ToolCall {
    type Error = ToolCallParseError;

    fn try_from((tool, raw): (Tool, Value)) -> Result<Self, Self::Error> {
        let kind = parse_kind(&tool, &raw)?;
        Ok(ToolCall { kind, raw })
    }
}

/// Parse a `(Tool, &Value)` into the appropriate `ToolCallKind`.
///
/// Each known tool deserializes via an intermediate `#[derive(Deserialize)]` struct
/// that handles provider-specific field aliases (e.g. `file_path` → `path`).
fn parse_kind(tool: &Tool, raw: &Value) -> Result<ToolCallKind, ToolCallParseError> {
    let err = |msg: String| ToolCallParseError {
        tool: tool.to_string(),
        message: msg,
    };

    match tool {
        Tool::Read => {
            let input: ReadInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Read { path: input.path })
        }
        Tool::Grep => {
            let input: GrepInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Grep {
                path: input.path,
                pattern: input.pattern,
            })
        }
        Tool::Glob => {
            let input: GlobInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Glob {
                target_directory: input.target_directory,
            })
        }
        Tool::SemanticSearch => {
            let input: SemanticSearchInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::SemanticSearch {
                target_directories: input.target_directories,
            })
        }
        Tool::Write => {
            let input: WriteInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Write {
                path: input.path,
                content: input.content,
            })
        }
        Tool::StrReplace => {
            let input: StrReplaceInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::StrReplace {
                path: input.path,
                old_string: input.old_string,
                new_string: input.new_string,
            })
        }
        Tool::Delete => {
            let input: DeleteInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Delete { path: input.path })
        }
        Tool::Edit => {
            let input: EditInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Edit {
                path: input.path,
                old_content: input.old_content,
                new_content: input.new_content,
            })
        }
        Tool::MultiEdit => {
            let input: MultiEditInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::MultiEdit {
                path: input.path,
                edits: input
                    .edits
                    .into_iter()
                    .map(|e| MultiEditEntry {
                        old_string: e.old_string,
                        new_string: e.new_string,
                    })
                    .collect(),
            })
        }
        Tool::EditNotebook => Ok(ToolCallKind::EditNotebook {}),
        Tool::NotebookEdit => Ok(ToolCallKind::NotebookEdit {}),
        Tool::Bash => {
            let input: BashInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::Bash {
                command: input.command,
            })
        }
        Tool::Task => Ok(ToolCallKind::Task {}),
        Tool::TodoWrite => Ok(ToolCallKind::TodoWrite {}),
        Tool::WebFetch => {
            let input: WebFetchInput =
                serde_json::from_value(raw.clone()).map_err(|e| err(e.to_string()))?;
            Ok(ToolCallKind::WebFetch { url: input.url })
        }
        Tool::Unknown(name) => Ok(ToolCallKind::Unknown { name: name.clone() }),
    }
}

// ===========================================================================
// Intermediate deserialization structs (private)
//
// These exist solely to handle provider-specific field aliases during
// construction. The aliases (e.g. `file_path` for `path`) are needed because
// different providers send different field names for the same logical field.
// ===========================================================================

#[derive(Deserialize)]
struct ReadInput {
    #[serde(alias = "file_path")]
    path: String,
}

#[derive(Deserialize)]
struct GrepInput {
    #[serde(alias = "file_path")]
    path: Option<String>,
    pattern: String,
}

#[derive(Deserialize)]
struct GlobInput {
    target_directory: String,
}

#[derive(Deserialize)]
struct SemanticSearchInput {
    target_directories: Vec<String>,
}

#[derive(Deserialize)]
struct WriteInput {
    #[serde(alias = "file_path")]
    path: String,
    content: Option<String>,
}

#[derive(Deserialize)]
struct StrReplaceInput {
    #[serde(alias = "file_path")]
    path: Option<String>,
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
struct DeleteInput {
    #[serde(alias = "file_path")]
    path: String,
}

#[derive(Deserialize)]
struct EditInput {
    #[serde(alias = "file_path")]
    path: Option<String>,
    old_content: String,
    new_content: String,
}

#[derive(Deserialize)]
struct MultiEditInput {
    #[serde(alias = "file_path")]
    path: Option<String>,
    edits: Vec<MultiEditEntryInput>,
}

#[derive(Deserialize)]
struct MultiEditEntryInput {
    old_string: String,
    new_string: String,
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
}

#[derive(Deserialize)]
struct WebFetchInput {
    url: String,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Construction: known tools ---

    #[test]
    fn read_tool_parses() {
        let raw = json!({"path": "/home/user/file.rs"});
        let tc = ToolCall::try_from((Tool::Read, raw.clone())).unwrap();
        assert_eq!(tc.tool(), Tool::Read);
        assert_eq!(tc.tool_name(), "Read");
        assert_eq!(tc.raw_input(), &raw);
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Read {
                path: "/home/user/file.rs".into()
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/home/user/file.rs"]);
    }

    #[test]
    fn read_tool_file_path_alias() {
        let raw = json!({"file_path": "/tmp/test.txt"});
        let tc = ToolCall::try_from((Tool::Read, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Read {
                path: "/tmp/test.txt".into()
            }
        );
    }

    #[test]
    fn grep_tool_with_path() {
        let raw = json!({"pattern": "TODO", "path": "/home/user/project"});
        let tc = ToolCall::try_from((Tool::Grep, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Grep {
                path: Some("/home/user/project".into()),
                pattern: "TODO".into(),
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/home/user/project"]);
    }

    #[test]
    fn grep_tool_without_path() {
        let raw = json!({"pattern": "TODO"});
        let tc = ToolCall::try_from((Tool::Grep, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Grep {
                path: None,
                pattern: "TODO".into(),
            }
        );
        assert!(tc.matchable_args().is_empty());
    }

    #[test]
    fn glob_tool_parses() {
        let raw = json!({"target_directory": "/home/user/project"});
        let tc = ToolCall::try_from((Tool::Glob, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Glob {
                target_directory: "/home/user/project".into()
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/home/user/project"]);
    }

    #[test]
    fn semantic_search_parses() {
        let raw = json!({"target_directories": ["/a", "/b"], "query": "auth"});
        let tc = ToolCall::try_from((Tool::SemanticSearch, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::SemanticSearch {
                target_directories: vec!["/a".into(), "/b".into()]
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/a", "/b"]);
    }

    #[test]
    fn write_tool_parses() {
        let raw = json!({"path": "/tmp/out.txt", "content": "hello"});
        let tc = ToolCall::try_from((Tool::Write, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Write {
                path: "/tmp/out.txt".into(),
                content: Some("hello".into()),
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/tmp/out.txt"]);
    }

    #[test]
    fn write_tool_file_path_alias() {
        let raw = json!({"file_path": "/tmp/out.txt", "content": "hello"});
        let tc = ToolCall::try_from((Tool::Write, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Write {
                path: "/tmp/out.txt".into(),
                content: Some("hello".into()),
            }
        );
    }

    #[test]
    fn str_replace_tool_with_path() {
        let raw = json!({
            "path": "/tmp/file.rs",
            "old_string": "foo",
            "new_string": "bar"
        });
        let tc = ToolCall::try_from((Tool::StrReplace, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::StrReplace {
                path: Some("/tmp/file.rs".into()),
                old_string: "foo".into(),
                new_string: "bar".into(),
            }
        );
        assert_eq!(tc.matchable_args(), vec!["/tmp/file.rs"]);
    }

    #[test]
    fn str_replace_tool_without_path() {
        let raw = json!({"old_string": "foo", "new_string": "bar"});
        let tc = ToolCall::try_from((Tool::StrReplace, raw)).unwrap();
        assert!(tc.matchable_args().is_empty());
    }

    #[test]
    fn delete_tool_parses() {
        let raw = json!({"path": "/tmp/old.txt"});
        let tc = ToolCall::try_from((Tool::Delete, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Delete {
                path: "/tmp/old.txt".into()
            }
        );
    }

    #[test]
    fn edit_tool_parses() {
        let raw = json!({
            "path": "/tmp/file.rs",
            "old_content": "fn old()",
            "new_content": "fn new()"
        });
        let tc = ToolCall::try_from((Tool::Edit, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Edit {
                path: Some("/tmp/file.rs".into()),
                old_content: "fn old()".into(),
                new_content: "fn new()".into(),
            }
        );
    }

    #[test]
    fn multi_edit_tool_parses() {
        let raw = json!({
            "path": "/tmp/file.rs",
            "edits": [
                {"old_string": "a", "new_string": "b"},
                {"old_string": "c", "new_string": "d"}
            ]
        });
        let tc = ToolCall::try_from((Tool::MultiEdit, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::MultiEdit {
                path: Some("/tmp/file.rs".into()),
                edits: vec![
                    MultiEditEntry {
                        old_string: "a".into(),
                        new_string: "b".into()
                    },
                    MultiEditEntry {
                        old_string: "c".into(),
                        new_string: "d".into()
                    },
                ],
            }
        );
    }

    #[test]
    fn bash_tool_parses() {
        let raw = json!({"command": "ls -la"});
        let tc = ToolCall::try_from((Tool::Bash, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Bash {
                command: "ls -la".into()
            }
        );
        assert_eq!(tc.matchable_args(), vec!["ls -la"]);
    }

    #[test]
    fn web_fetch_tool_parses() {
        let raw = json!({"url": "https://example.com"});
        let tc = ToolCall::try_from((Tool::WebFetch, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::WebFetch {
                url: "https://example.com".into()
            }
        );
        assert_eq!(tc.matchable_args(), vec!["https://example.com"]);
    }

    // --- Construction: empty-body tools ---

    #[test]
    fn task_tool_ignores_args() {
        let raw = json!({"description": "something", "prompt": "do stuff"});
        let tc = ToolCall::try_from((Tool::Task, raw.clone())).unwrap();
        assert_eq!(*tc.kind(), ToolCallKind::Task {});
        assert!(tc.matchable_args().is_empty());
        // Raw blob is still preserved
        assert_eq!(tc.raw_input(), &raw);
    }

    #[test]
    fn todo_write_tool_ignores_args() {
        let raw = json!({"todos": [{"content": "x", "status": "pending"}]});
        let tc = ToolCall::try_from((Tool::TodoWrite, raw)).unwrap();
        assert_eq!(*tc.kind(), ToolCallKind::TodoWrite {});
    }

    #[test]
    fn edit_notebook_ignores_args() {
        let raw = json!({"target_notebook": "analysis.ipynb"});
        let tc = ToolCall::try_from((Tool::EditNotebook, raw.clone())).unwrap();
        assert_eq!(*tc.kind(), ToolCallKind::EditNotebook {});
        assert_eq!(tc.raw_input(), &raw);
    }

    #[test]
    fn notebook_edit_ignores_args() {
        let raw = json!({"target_notebook": "analysis.ipynb"});
        let tc = ToolCall::try_from((Tool::NotebookEdit, raw)).unwrap();
        assert_eq!(*tc.kind(), ToolCallKind::NotebookEdit {});
    }

    // --- Construction: unknown tools ---

    #[test]
    fn unknown_tool_preserves_name() {
        let raw = json!({"whatever": "data", "nested": {"key": 42}});
        let tc = ToolCall::try_from((Tool::Unknown("my_mcp_tool".into()), raw.clone())).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Unknown {
                name: "my_mcp_tool".into()
            }
        );
        assert_eq!(tc.tool(), Tool::Unknown("my_mcp_tool".into()));
        assert_eq!(tc.tool_name(), "my_mcp_tool");
        assert!(tc.matchable_args().is_empty());
        assert_eq!(tc.raw_input(), &raw);
    }

    // --- Construction: error cases ---

    #[test]
    fn read_tool_missing_path_errors() {
        let raw = json!({"content": "no path here"});
        let result = ToolCall::try_from((Tool::Read, raw));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.tool, "Read");
        assert!(
            err.message.contains("path"),
            "error should mention missing field: {}",
            err.message
        );
    }

    #[test]
    fn write_tool_missing_content_parses_as_none() {
        let raw = json!({"path": "/tmp/file.txt"});
        let tc = ToolCall::try_from((Tool::Write, raw)).unwrap();
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Write {
                path: "/tmp/file.txt".into(),
                content: None,
            }
        );
    }

    #[test]
    fn bash_tool_missing_command_errors() {
        let raw = json!({"script": "echo hi"});
        let result = ToolCall::try_from((Tool::Bash, raw));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().tool, "Bash");
    }

    // --- tool() round-trips through Tool enum ---

    #[test]
    fn tool_roundtrip_all_known() {
        let cases: Vec<(Tool, Value)> = vec![
            (Tool::Read, json!({"path": "/a"})),
            (Tool::Grep, json!({"pattern": "x"})),
            (Tool::Glob, json!({"target_directory": "/a"})),
            (Tool::SemanticSearch, json!({"target_directories": ["/a"]})),
            (Tool::Write, json!({"path": "/a", "content": "c"})),
            (
                Tool::StrReplace,
                json!({"old_string": "a", "new_string": "b"}),
            ),
            (Tool::Delete, json!({"path": "/a"})),
            (Tool::Edit, json!({"old_content": "a", "new_content": "b"})),
            (
                Tool::MultiEdit,
                json!({"edits": [{"old_string": "a", "new_string": "b"}]}),
            ),
            (Tool::EditNotebook, json!({})),
            (Tool::NotebookEdit, json!({})),
            (Tool::Bash, json!({"command": "ls"})),
            (Tool::Task, json!({})),
            (Tool::TodoWrite, json!({})),
            (Tool::WebFetch, json!({"url": "https://x.com"})),
        ];

        for (tool, raw) in cases {
            let tc = ToolCall::try_from((tool.clone(), raw)).unwrap();
            assert_eq!(tc.tool(), tool, "tool() mismatch for {:?}", tc.kind());
            assert_eq!(tc.tool_name(), tool.as_str());
        }
    }

    // --- matchable_args consistency ---

    #[test]
    fn matchable_args_file_path_alias_write() {
        // Alias should resolve to the same matchable arg
        let tc1 = ToolCall::try_from((Tool::Write, json!({"path": "/a", "content": "c"}))).unwrap();
        let tc2 =
            ToolCall::try_from((Tool::Write, json!({"file_path": "/a", "content": "c"}))).unwrap();
        assert_eq!(tc1.matchable_args(), tc2.matchable_args());
    }

    #[test]
    fn matchable_args_delete_file_path_alias() {
        let tc = ToolCall::try_from((Tool::Delete, json!({"file_path": "/tmp/gone.txt"}))).unwrap();
        assert_eq!(tc.matchable_args(), vec!["/tmp/gone.txt"]);
    }

    // --- Extra fields in raw are preserved but ignored ---

    #[test]
    fn extra_fields_preserved_in_raw() {
        let raw = json!({
            "path": "/a",
            "content": "c",
            "extra_provider_field": true,
            "metadata": {"version": 2}
        });
        let tc = ToolCall::try_from((Tool::Write, raw.clone())).unwrap();
        // Typed fields extracted correctly
        assert_eq!(
            *tc.kind(),
            ToolCallKind::Write {
                path: "/a".into(),
                content: Some("c".into())
            }
        );
        // Raw preserves everything
        assert_eq!(tc.raw_input()["extra_provider_field"], json!(true));
        assert_eq!(tc.raw_input()["metadata"]["version"], json!(2));
    }
}

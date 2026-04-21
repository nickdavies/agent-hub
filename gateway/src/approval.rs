use protocol::{ApprovalStatus, ExtraContext, ToolCallKind};
use similar::{ChangeTag, TextDiff};

use crate::types::{DecisionStatus, HookOutput};

pub(crate) fn status_to_output(status: &ApprovalStatus) -> HookOutput {
    match status {
        ApprovalStatus::Approved { message } => HookOutput {
            status: DecisionStatus::Approved,
            message: message.clone(),
        },
        ApprovalStatus::Denied { reason } => HookOutput {
            status: if reason.is_empty() {
                DecisionStatus::Denied
            } else {
                DecisionStatus::DeniedWithReason(reason.clone())
            },
            message: None,
        },
        ApprovalStatus::Cancelled => HookOutput {
            status: DecisionStatus::Denied,
            message: None,
        },
        ApprovalStatus::Pending => HookOutput {
            // Should not happen — but if we get here, deny as a safety net.
            status: DecisionStatus::Denied,
            message: None,
        },
    }
}

/// Build the `extra` context blob that is sent to the server for human review.
///
/// For FileWrite tools this is a unified diff so the reviewer can see exactly
/// what would change. For shell calls escalated by Dippy the delegate's reason
/// is forwarded. All other tools return None.
pub(crate) fn collect_extra(
    tool_call: &protocol::ToolCall,
    dippy_reason: Option<&str>,
) -> Option<ExtraContext> {
    match tool_call.kind() {
        ToolCallKind::Write { path, content } => {
            let old_content = std::fs::read_to_string(path).unwrap_or_default();
            match content {
                Some(content) => {
                    let diff = make_unified_diff(&old_content, content, path);
                    Some(ExtraContext::Diff { diff })
                }
                None => None,
            }
        }
        ToolCallKind::StrReplace {
            path,
            old_string,
            new_string,
        } => {
            let label = path.as_deref().unwrap_or("file");
            let diff = make_unified_diff(old_string, new_string, label);
            Some(ExtraContext::Diff { diff })
        }
        ToolCallKind::Edit {
            path,
            old_content,
            new_content,
        } => {
            let label = path.as_deref().unwrap_or("file");
            let diff = make_unified_diff(old_content, new_content, label);
            Some(ExtraContext::Diff { diff })
        }
        ToolCallKind::MultiEdit { path, edits } => {
            let label = path.as_deref().unwrap_or("file");
            let combined: String = edits
                .iter()
                .map(|edit| make_unified_diff(&edit.old_string, &edit.new_string, label))
                .collect::<Vec<_>>()
                .join("\n");
            if combined.is_empty() {
                return None;
            }
            Some(ExtraContext::Diff { diff: combined })
        }
        _ => {
            let reason = dippy_reason?;
            Some(ExtraContext::DippyReason {
                dippy_reason: reason.to_string(),
            })
        }
    }
}

/// Produce a unified diff string from two text blobs.
pub(crate) fn make_unified_diff(old: &str, new: &str, label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- {label}\n+++ {label}\n");
    for group in diff.grouped_ops(3) {
        for op in &group {
            for change in diff.iter_changes(op) {
                let prefix = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(prefix);
                out.push_str(change.value());
                if change.missing_newline() {
                    out.push('\n');
                }
            }
        }
    }
    out
}

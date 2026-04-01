use std::io::{self, Write};

use chrono::Utc;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

use crate::client::Approval;

/// What the user wants to do after a UI event.
pub enum Action {
    /// Approve the approval at the given index.
    Approve(usize),
    /// Deny the approval at the given index — enters reason input mode.
    StartDeny(usize),
    /// Navigate to the next item.
    Next,
    /// Navigate to the previous item.
    Prev,
    /// Toggle raw view of tool input.
    ToggleRaw,
    /// Scroll content down by one line, falling through to Next at bottom.
    LineDown,
    /// Scroll content up by one line, falling through to Prev at top.
    LineUp,
    /// Scroll expanded detail down by half a page.
    ScrollDown,
    /// Scroll expanded detail up by half a page.
    ScrollUp,
    /// Quit the application.
    Quit,
    /// No action (unrecognized key, etc).
    None,
}

/// Mode of the UI.
pub enum Mode {
    /// Normal browsing mode.
    Normal,
    /// Typing a deny reason for the approval at the given index.
    DenyInput { index: usize, buffer: String },
}

/// Result of processing a key event in DenyInput mode.
pub enum DenyInputResult {
    /// User confirmed the deny reason.
    Confirm { index: usize, reason: String },
    /// User cancelled.
    Cancel,
    /// Still typing.
    Continue,
}

#[derive(Clone, Copy)]
enum LineKind {
    /// Default color for raw tool_input key-value pairs.
    Normal,
    /// Green for diff insertions and new-file content.
    DiffAdd,
    /// Red for diff deletions.
    DiffRemove,
    /// Dark grey for diff context lines and "(no changes)".
    DiffContext,
    /// Cyan for @@ ... @@ hunk headers.
    DiffHunkHeader,
    /// Blue for path headers and context labels.
    Info,
    /// Empty line with just the │ bar.
    Separator,
}

struct ExpandedLine {
    text: String,
    kind: LineKind,
}

pub struct Ui {
    pub selected: usize,
    pub mode: Mode,
    pub status_message: Option<String>,
    pub show_raw: bool,
    pub scroll_offset: usize,
    /// Total lines of expanded content for the currently rendered approval.
    pub last_content_lines: usize,
    /// Visible rows available for expanded content in the last render.
    pub last_visible_rows: usize,
    server_url: String,
}

impl Ui {
    pub fn new(server_url: &str) -> Self {
        Self {
            selected: 0,
            mode: Mode::Normal,
            status_message: None,
            show_raw: false,
            scroll_offset: 0,
            last_content_lines: 0,
            last_visible_rows: 0,
            server_url: server_url.to_string(),
        }
    }

    pub fn enter(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        Ok(())
    }

    pub fn leave(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen
        )?;
        terminal::disable_raw_mode()?;
        Ok(())
    }

    /// Clamp selection index to valid range.
    pub fn clamp_selection(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Handle a key event in normal mode. Returns the action to take.
    pub fn handle_normal_key(&self, key: KeyEvent, approval_count: usize) -> Action {
        if approval_count == 0 {
            return match key.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Action::ScrollDown
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::ScrollUp,
            KeyCode::Char('a') | KeyCode::Char('1') => Action::Approve(self.selected),
            KeyCode::Char('d') | KeyCode::Char('2') => Action::StartDeny(self.selected),
            KeyCode::Char('r') => Action::ToggleRaw,
            KeyCode::Down | KeyCode::Char('j') => Action::LineDown,
            KeyCode::Up | KeyCode::Char('k') => Action::LineUp,
            KeyCode::Tab => Action::Next,
            KeyCode::BackTab => Action::Prev,
            KeyCode::PageDown => Action::ScrollDown,
            KeyCode::PageUp => Action::ScrollUp,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            _ => Action::None,
        }
    }

    /// Handle a key event in deny input mode. Mutates the buffer and returns result.
    pub fn handle_deny_key(&mut self, key: KeyEvent) -> DenyInputResult {
        let Mode::DenyInput { index, buffer } = &mut self.mode else {
            return DenyInputResult::Cancel;
        };
        let index = *index;

        match key.code {
            KeyCode::Enter => {
                let reason = std::mem::take(buffer);
                DenyInputResult::Confirm { index, reason }
            }
            KeyCode::Esc => DenyInputResult::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                DenyInputResult::Cancel
            }
            KeyCode::Backspace => {
                buffer.pop();
                DenyInputResult::Continue
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                DenyInputResult::Continue
            }
            _ => DenyInputResult::Continue,
        }
    }

    /// Render the full UI to the terminal.
    pub fn render(&mut self, approvals: &[Approval]) -> io::Result<()> {
        let mut stdout = io::stdout();
        let (cols, rows) = terminal::size()?;
        let width = cols as usize;
        let max_content_row = (rows as usize).saturating_sub(3);
        let separator: String = "\u{2501}".repeat(width);
        let max_line_width = width.saturating_sub(6);

        queue!(stdout, MoveTo(0, 0))?;
        let mut row: usize = 0;
        let mut content_total: usize = 0;
        let mut content_visible: usize = 0;

        // Header
        let pending_text = format!("{} pending", approvals.len());
        let header_left = format!("Claude Review \u{2014} {}", self.server_url);
        let pad = width.saturating_sub(header_left.len() + pending_text.len());
        queue!(
            stdout,
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold),
            Print(&header_left),
            Print(" ".repeat(pad)),
            Print(&pending_text),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::UntilNewLine),
            Print("\r\n"),
        )?;
        row += 1;

        // Separator
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(&separator),
            ResetColor,
            Print("\r\n"),
        )?;
        row += 1;

        if approvals.is_empty() {
            if row < max_content_row {
                queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
                row += 1;
            }
            if row < max_content_row {
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print("  Waiting for approvals..."),
                    ResetColor,
                    Clear(ClearType::UntilNewLine),
                    Print("\r\n"),
                )?;
                row += 1;
            }
        } else {
            let now = Utc::now();
            for (i, approval) in approvals.iter().enumerate() {
                if row >= max_content_row {
                    break;
                }
                let is_selected = i == self.selected;
                let age = format_age(now, approval.created_at);

                // Spacer line
                queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
                row += 1;
                if row >= max_content_row {
                    break;
                }

                // Summary line
                if is_selected {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Yellow),
                        SetAttribute(Attribute::Bold),
                        Print("\u{25b8} "),
                    )?;
                } else {
                    queue!(stdout, Print("  "))?;
                }

                let summary = format!("{} \u{203a} {}", approval.project, approval.tool_name);
                let summary_pad = width.saturating_sub(summary.len() + age.len() + 4);
                if is_selected {
                    queue!(
                        stdout,
                        Print(&summary),
                        SetAttribute(Attribute::Reset),
                        ResetColor,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" ".repeat(summary_pad)),
                        Print(&age),
                        ResetColor,
                    )?;
                } else {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::White),
                        Print(&summary),
                        ResetColor,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" ".repeat(summary_pad)),
                        Print(&age),
                        ResetColor,
                    )?;
                }
                queue!(stdout, Clear(ClearType::UntilNewLine), Print("\r\n"))?;
                row += 1;

                if is_selected {
                    let lines = build_expanded_lines(approval, max_line_width, self.show_raw);
                    content_total = lines.len();
                    self.last_content_lines = content_total;
                    let remaining_rows = max_content_row.saturating_sub(row);
                    self.last_visible_rows = remaining_rows;
                    let max_offset = content_total.saturating_sub(remaining_rows.max(1));
                    self.scroll_offset = self.scroll_offset.min(max_offset);
                    for line in lines.iter().skip(self.scroll_offset) {
                        if row >= max_content_row {
                            break;
                        }
                        render_line(&mut stdout, line)?;
                        row += 1;
                        content_visible += 1;
                    }
                }
            }
        }

        // Clear remaining content area
        while row < max_content_row {
            queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
            row += 1;
        }

        // Status bar
        queue!(stdout, MoveTo(0, max_content_row as u16))?;

        if let Some(msg) = &self.status_message {
            queue!(
                stdout,
                SetForegroundColor(Color::Red),
                Print("  "),
                Print(msg),
                ResetColor,
                Clear(ClearType::UntilNewLine),
                Print("\r\n"),
            )?;
        } else if content_total > content_visible {
            let start = self.scroll_offset + 1;
            let end = (self.scroll_offset + content_visible).min(content_total);
            let indicator = format!("lines {start}-{end} of {content_total}");
            let ipad = width.saturating_sub(indicator.len() + 2);
            queue!(
                stdout,
                Print(" ".repeat(ipad)),
                SetForegroundColor(Color::DarkGrey),
                Print(&indicator),
                ResetColor,
                Clear(ClearType::UntilNewLine),
                Print("\r\n"),
            )?;
        } else {
            queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
        }

        // Separator
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(&separator),
            ResetColor,
            Print("\r\n"),
        )?;

        // Key hints or deny input
        match &self.mode {
            Mode::Normal => {
                if approvals.is_empty() {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" (q)uit"),
                        ResetColor,
                        Clear(ClearType::UntilNewLine),
                    )?;
                } else {
                    queue!(
                        stdout,
                        Print(" "),
                        SetForegroundColor(Color::Green),
                        Print("(a)"),
                        ResetColor,
                        Print("pprove  "),
                        SetForegroundColor(Color::Red),
                        Print("(d)"),
                        ResetColor,
                        Print("eny  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(r)"),
                        ResetColor,
                        Print("aw  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(\u{2191}\u{2193})"),
                        ResetColor,
                        Print(" scroll  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(Tab)"),
                        ResetColor,
                        Print(" next  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(q)"),
                        ResetColor,
                        Print("uit"),
                        Clear(ClearType::UntilNewLine),
                    )?;
                }
            }
            Mode::DenyInput { buffer, .. } => {
                queue!(
                    stdout,
                    Print(" Reason (Enter to confirm, Esc to cancel): "),
                    SetForegroundColor(Color::Yellow),
                    Print(buffer),
                    Print("\u{2588}"),
                    ResetColor,
                    Clear(ClearType::UntilNewLine),
                )?;
            }
        }

        stdout.flush()?;
        Ok(())
    }
}

/// Render a single expanded-detail line with the │ prefix.
fn render_line(stdout: &mut io::Stdout, line: &ExpandedLine) -> io::Result<()> {
    if matches!(line.kind, LineKind::Separator) {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  \u{2502}"),
            ResetColor,
            Clear(ClearType::UntilNewLine),
            Print("\r\n"),
        )?;
        return Ok(());
    }

    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("  \u{2502} "),
    )?;

    match line.kind {
        LineKind::Normal => queue!(stdout, ResetColor)?,
        LineKind::DiffAdd => queue!(stdout, SetForegroundColor(Color::Green))?,
        LineKind::DiffRemove => queue!(stdout, SetForegroundColor(Color::Red))?,
        LineKind::DiffContext => queue!(stdout, SetForegroundColor(Color::DarkGrey))?,
        LineKind::DiffHunkHeader => queue!(stdout, SetForegroundColor(Color::Cyan))?,
        LineKind::Info => queue!(stdout, SetForegroundColor(Color::Blue))?,
        LineKind::Separator => unreachable!(),
    }

    queue!(
        stdout,
        Print(&line.text),
        ResetColor,
        Clear(ClearType::UntilNewLine),
        Print("\r\n"),
    )?;

    Ok(())
}

/// Build all expanded-detail lines for a selected approval.
fn build_expanded_lines(
    approval: &Approval,
    max_line_width: usize,
    show_raw: bool,
) -> Vec<ExpandedLine> {
    let mut lines = Vec::new();

    lines.push(ExpandedLine {
        text: truncate_str(&approval.session_display_name, max_line_width),
        kind: LineKind::Info,
    });

    if show_raw {
        let json = serde_json::to_string_pretty(&approval.tool_input).unwrap_or_default();
        for line in json.lines() {
            lines.push(ExpandedLine {
                text: truncate_str(line, max_line_width),
                kind: LineKind::Normal,
            });
        }
    } else if approval.tool_name == "Write" {
        build_write_diff_lines(&approval.tool_input, max_line_width, &mut lines);
    } else {
        let input_str = format_tool_input(&approval.tool_input);
        for line in input_str.lines() {
            lines.push(ExpandedLine {
                text: truncate_str(line, max_line_width),
                kind: LineKind::Normal,
            });
        }
    }

    if let Some(extra) = &approval.context.extra {
        let text = if let Some(reason) = extra
            .get("dippy_reason")
            .and_then(|v: &serde_json::Value| v.as_str())
        {
            format!("Dippy: {reason}")
        } else {
            format!("Context: {}", extra)
        };
        lines.push(ExpandedLine {
            text: String::new(),
            kind: LineKind::Separator,
        });
        lines.push(ExpandedLine {
            text: truncate_str(&text, max_line_width),
            kind: LineKind::Info,
        });
    }

    lines
}

/// Build diff lines for a Write tool request.
fn build_write_diff_lines(
    tool_input: &serde_json::Value,
    max_line_width: usize,
    lines: &mut Vec<ExpandedLine>,
) {
    use similar::{ChangeTag, TextDiff};

    let path = tool_input
        .get("path")
        .or_else(|| tool_input.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let new_contents = tool_input
        .get("contents")
        .or_else(|| tool_input.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let path_display = truncate_str(path, max_line_width.saturating_sub(6));
    lines.push(ExpandedLine {
        text: format!("path: {path_display}"),
        kind: LineKind::Info,
    });

    let file_exists = std::path::Path::new(path).exists();
    let old_contents = if file_exists {
        std::fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    if old_contents == new_contents {
        lines.push(ExpandedLine {
            text: "(no changes)".to_string(),
            kind: LineKind::DiffContext,
        });
        return;
    }

    if !file_exists {
        lines.push(ExpandedLine {
            text: format!("(new file, {} lines)", new_contents.lines().count()),
            kind: LineKind::DiffAdd,
        });
    }

    let diff = TextDiff::from_lines(old_contents.as_str(), new_contents);
    let udiff = diff.unified_diff();
    for hunk in udiff.iter_hunks() {
        let header = hunk.header().to_string();
        lines.push(ExpandedLine {
            text: truncate_str(header.trim_end(), max_line_width),
            kind: LineKind::DiffHunkHeader,
        });

        for change in hunk.iter_changes() {
            let (prefix, kind) = match change.tag() {
                ChangeTag::Delete => ("-", LineKind::DiffRemove),
                ChangeTag::Insert => ("+", LineKind::DiffAdd),
                ChangeTag::Equal => (" ", LineKind::DiffContext),
            };
            let line_content = change.value().trim_end_matches('\n').trim_end_matches('\r');
            lines.push(ExpandedLine {
                text: truncate_str(&format!("{prefix}{line_content}"), max_line_width),
                kind,
            });
        }
    }
}

fn truncate_str(s: &str, max_width: usize) -> String {
    if s.len() <= max_width {
        return s.to_string();
    }
    let end = s.floor_char_boundary(max_width.saturating_sub(1));
    format!("{}\u{2026}", &s[..end])
}

/// Format tool_input as readable text. Shows key: value pairs for objects.
fn format_tool_input(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => {
            let mut lines = Vec::new();
            for (key, value) in map {
                let val_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                lines.push(format!("{key}: {val_str}"));
            }
            lines.join("\n")
        }
        other => other.to_string(),
    }
}

/// Format a duration between now and a timestamp as a human-readable age.
fn format_age(now: chrono::DateTime<Utc>, created: chrono::DateTime<Utc>) -> String {
    let secs = (now - created).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Poll for a terminal event with the given timeout.
pub fn poll_event(timeout: std::time::Duration) -> io::Result<Option<Event>> {
    if event::poll(timeout)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}

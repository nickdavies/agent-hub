use std::io::{self, Write};

use chrono::Utc;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

use crate::client::{Approval, Question};
use protocol::tool_call::{ToolCall, ToolCallKind};
use uuid::Uuid;

// ===========================================================================
// ReviewItem — unified item type for the TUI list
// ===========================================================================

/// A single item shown in the review list (either an approval or a question).
pub enum ReviewItem {
    Approval(Approval),
    Question(Question),
}

impl ReviewItem {
    pub fn id(&self) -> Uuid {
        match self {
            ReviewItem::Approval(a) => a.id,
            ReviewItem::Question(q) => q.id,
        }
    }

    pub fn created_at(&self) -> chrono::DateTime<Utc> {
        match self {
            ReviewItem::Approval(a) => a.created_at,
            ReviewItem::Question(q) => q.created_at,
        }
    }
}

// ===========================================================================
// Action
// ===========================================================================

/// What the user wants to do after a UI event.
pub enum Action {
    /// Approve the approval at the given index.
    Approve(usize),
    /// Deny the approval at the given index — enters reason input mode.
    StartDeny(usize),
    /// Open the answer-question dialog for the question at the given index.
    StartAnswer(usize),
    /// Reject the question at the given index.
    RejectQuestion(usize),
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

// ===========================================================================
// Mode
// ===========================================================================

/// Mode of the UI.
pub enum Mode {
    /// Normal browsing mode.
    Normal,
    /// Typing a deny reason for the approval at the given index.
    DenyInput { index: usize, buffer: String },
    /// Answering a multi-step question.
    AnswerQuestion {
        /// Index in the items list of the question being answered.
        #[allow(dead_code)]
        index: usize,
        /// The question data.
        question: Box<Question>,
        /// Which sub-question we are currently on (0-based).
        q_idx: usize,
        /// For each sub-question: which option index is selected (None = none yet).
        selections: Vec<Option<usize>>,
        /// For each sub-question: free-text custom answer buffer.
        custom_inputs: Vec<String>,
    },
}

// ===========================================================================
// Result types
// ===========================================================================

/// Result of processing a key event in DenyInput mode.
pub enum DenyInputResult {
    /// User confirmed the deny reason.
    Confirm { index: usize, reason: String },
    /// User cancelled.
    Cancel,
    /// Still typing.
    Continue,
}

/// Result of processing a key event in AnswerQuestion mode.
pub enum AnswerQuestionResult {
    /// User submitted answers. `answers[i]` is the list of selected labels for question `i`.
    Submit { id: Uuid, answers: Vec<Vec<String>> },
    /// User rejected the question.
    Reject { id: Uuid },
    /// User cancelled (Esc).
    Cancel,
    /// Still interacting.
    Continue,
}

// ===========================================================================
// Internal line types
// ===========================================================================

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

// ===========================================================================
// Ui struct
// ===========================================================================

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
    pub fn handle_normal_key(&self, key: KeyEvent, items: &[ReviewItem]) -> Action {
        if items.is_empty() {
            return match key.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                _ => Action::None,
            };
        }

        // Dispatch based on the currently selected item type
        let is_question = matches!(items.get(self.selected), Some(ReviewItem::Question(_)));

        match key.code {
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Action::ScrollDown
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::ScrollUp,
            KeyCode::Char('a') | KeyCode::Char('1') if !is_question => {
                Action::Approve(self.selected)
            }
            KeyCode::Char('d') | KeyCode::Char('2') if !is_question => {
                Action::StartDeny(self.selected)
            }
            // For questions: Enter or 'a' opens the answer dialog
            KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('1') if is_question => {
                Action::StartAnswer(self.selected)
            }
            // For questions: 'x' / 'd' rejects
            KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Char('2') if is_question => {
                Action::RejectQuestion(self.selected)
            }
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

    /// Handle a key event in answer-question mode.
    pub fn handle_answer_key(&mut self, key: KeyEvent) -> AnswerQuestionResult {
        let Mode::AnswerQuestion {
            question,
            q_idx,
            selections,
            custom_inputs,
            ..
        } = &mut self.mode
        else {
            return AnswerQuestionResult::Cancel;
        };

        let q_id = question.id;
        let total_qs = question.questions.len();
        let current_q = &question.questions[*q_idx];
        let opt_count = current_q.options.len();
        let allows_custom = current_q.custom.unwrap_or(true);

        // The "custom" input slot is at index `opt_count` (after all options).
        // selections[q_idx] stores Some(opt_index) where opt_index == opt_count means custom.
        let custom_slot = opt_count;

        match key.code {
            KeyCode::Esc => return AnswerQuestionResult::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return AnswerQuestionResult::Cancel;
            }
            // Reject with 'x'
            KeyCode::Char('x') => {
                return AnswerQuestionResult::Reject { id: q_id };
            }
            // Navigate options with up/down or number keys
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = selections[*q_idx].unwrap_or(0);
                let max_slot = if allows_custom {
                    custom_slot
                } else {
                    opt_count.saturating_sub(1)
                };
                if cur > 0 {
                    selections[*q_idx] = Some(cur - 1);
                } else {
                    selections[*q_idx] = Some(max_slot);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = selections[*q_idx].unwrap_or(0);
                let max_slot = if allows_custom {
                    custom_slot
                } else {
                    opt_count.saturating_sub(1)
                };
                if cur < max_slot {
                    selections[*q_idx] = Some(cur + 1);
                } else {
                    selections[*q_idx] = Some(0);
                }
            }
            // Number key shortcuts (1-9)
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let n = (c as usize) - ('1' as usize);
                if n < opt_count {
                    selections[*q_idx] = Some(n);
                } else if allows_custom && n == opt_count {
                    selections[*q_idx] = Some(custom_slot);
                }
            }
            // Enter confirms selection for this sub-question and advances / submits
            KeyCode::Enter => {
                let sel = selections[*q_idx];
                let is_custom_selected = sel == Some(custom_slot) && allows_custom;
                if is_custom_selected {
                    // Custom text answer — only proceed if something was typed
                    if custom_inputs[*q_idx].is_empty() {
                        // Nothing typed yet, stay in this mode
                        return AnswerQuestionResult::Continue;
                    }
                } else if sel.is_none() && opt_count > 0 {
                    // Nothing selected yet, default to first option
                    selections[*q_idx] = Some(0);
                }

                if *q_idx + 1 < total_qs {
                    *q_idx += 1;
                    return AnswerQuestionResult::Continue;
                }

                // All sub-questions answered — build answers vector
                let answers: Vec<Vec<String>> = question
                    .questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        let sel = selections[i];
                        let is_custom = sel == Some(q.options.len()) && q.custom.unwrap_or(true);
                        if is_custom {
                            vec![custom_inputs[i].clone()]
                        } else if let Some(idx) = sel {
                            if idx < q.options.len() {
                                vec![q.options[idx].label.clone()]
                            } else {
                                vec![]
                            }
                        } else if !q.options.is_empty() {
                            // Default to first option
                            vec![q.options[0].label.clone()]
                        } else {
                            vec![]
                        }
                    })
                    .collect();

                return AnswerQuestionResult::Submit { id: q_id, answers };
            }
            // Backspace for custom input when custom slot is selected
            KeyCode::Backspace => {
                let sel = selections[*q_idx];
                if sel == Some(custom_slot) && allows_custom {
                    custom_inputs[*q_idx].pop();
                }
            }
            // Type characters into the custom input when custom slot is selected
            KeyCode::Char(c) => {
                let sel = selections[*q_idx];
                if sel == Some(custom_slot) && allows_custom {
                    custom_inputs[*q_idx].push(c);
                }
            }
            _ => {}
        }

        AnswerQuestionResult::Continue
    }

    /// Render the full UI to the terminal.
    pub fn render(&mut self, items: &[ReviewItem]) -> io::Result<()> {
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
        let approvals_count = items
            .iter()
            .filter(|i| matches!(i, ReviewItem::Approval(_)))
            .count();
        let questions_count = items
            .iter()
            .filter(|i| matches!(i, ReviewItem::Question(_)))
            .count();
        let pending_text = match (approvals_count, questions_count) {
            (0, 0) => "0 pending".to_string(),
            (a, 0) => format!("{a} approval{}", if a == 1 { "" } else { "s" }),
            (0, q) => format!("{q} question{}", if q == 1 { "" } else { "s" }),
            (a, q) => format!(
                "{a} approval{}, {q} question{}",
                if a == 1 { "" } else { "s" },
                if q == 1 { "" } else { "s" }
            ),
        };
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

        if items.is_empty() {
            if row < max_content_row {
                queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
                row += 1;
            }
            if row < max_content_row {
                queue!(
                    stdout,
                    SetForegroundColor(Color::DarkGrey),
                    Print("  Waiting for approvals or questions..."),
                    ResetColor,
                    Clear(ClearType::UntilNewLine),
                    Print("\r\n"),
                )?;
                row += 1;
            }
        } else {
            let now = Utc::now();
            for (i, item) in items.iter().enumerate() {
                if row >= max_content_row {
                    break;
                }
                let is_selected = i == self.selected;

                // Spacer line
                queue!(stdout, Clear(ClearType::CurrentLine), Print("\r\n"))?;
                row += 1;
                if row >= max_content_row {
                    break;
                }

                match item {
                    ReviewItem::Approval(approval) => {
                        let age = format_age(now, approval.created_at);

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

                        let summary = format!("{} \u{203a} {}", approval.project, approval.tool);
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
                            let lines =
                                build_expanded_lines(approval, max_line_width, self.show_raw);
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
                    ReviewItem::Question(q) => {
                        let age = format_age(now, q.created_at);

                        if is_selected {
                            queue!(
                                stdout,
                                SetForegroundColor(Color::Magenta),
                                SetAttribute(Attribute::Bold),
                                Print("\u{25b8} "),
                            )?;
                        } else {
                            queue!(stdout, Print("  "))?;
                        }

                        let first_q = q
                            .questions
                            .first()
                            .map(|qi| qi.header.as_str())
                            .unwrap_or("question");
                        let summary = format!("{} \u{203a} ? {}", q.session_display_name, first_q);
                        let summary_pad = width.saturating_sub(summary.len() + age.len() + 4);
                        if is_selected {
                            queue!(
                                stdout,
                                SetForegroundColor(Color::Magenta),
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
                            // Show question text + options
                            let lines = build_question_lines(q, max_line_width);
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

        // Key hints / deny input / answer question
        match &self.mode {
            Mode::Normal => {
                let selected_is_question =
                    matches!(items.get(self.selected), Some(ReviewItem::Question(_)));

                if items.is_empty() {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" (q)uit"),
                        ResetColor,
                        Clear(ClearType::UntilNewLine),
                    )?;
                } else if selected_is_question {
                    queue!(
                        stdout,
                        Print(" "),
                        SetForegroundColor(Color::Magenta),
                        Print("(Enter)"),
                        ResetColor,
                        Print(" answer  "),
                        SetForegroundColor(Color::Red),
                        Print("(x)"),
                        ResetColor,
                        Print("reject  "),
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
            Mode::AnswerQuestion {
                question,
                q_idx,
                selections,
                custom_inputs,
                ..
            } => {
                let qi = &question.questions[*q_idx];
                let total = question.questions.len();
                let step_info = if total > 1 {
                    format!(" [{}/{}]", q_idx + 1, total)
                } else {
                    String::new()
                };
                let sel = selections[*q_idx];
                let opt_count = qi.options.len();
                let allows_custom = qi.custom.unwrap_or(true);
                let custom_slot = opt_count;
                let is_custom = sel == Some(custom_slot) && allows_custom;

                if is_custom {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Magenta),
                        Print(&format!(" {}{}> ", qi.header, step_info)),
                        ResetColor,
                        SetForegroundColor(Color::Yellow),
                        Print(&custom_inputs[*q_idx]),
                        Print("\u{2588}"),
                        ResetColor,
                        Print("  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(Enter) confirm  (Esc) cancel  (x) reject"),
                        ResetColor,
                        Clear(ClearType::UntilNewLine),
                    )?;
                } else {
                    let opt_label = sel
                        .and_then(|i| qi.options.get(i))
                        .map(|o| o.label.as_str())
                        .unwrap_or("(none)");
                    queue!(
                        stdout,
                        SetForegroundColor(Color::Magenta),
                        Print(&format!(" {}{}> ", qi.header, step_info)),
                        ResetColor,
                        SetForegroundColor(Color::Yellow),
                        Print(opt_label),
                        ResetColor,
                        Print("  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print(
                            "(\u{2191}\u{2193}) select  (Enter) confirm  (Esc) cancel  (x) reject"
                        ),
                        ResetColor,
                        Clear(ClearType::UntilNewLine),
                    )?;
                }
            }
        }

        stdout.flush()?;
        Ok(())
    }
}

// ===========================================================================
// Line builders
// ===========================================================================

/// Build expanded-detail lines for a pending question.
fn build_question_lines(q: &Question, max_line_width: usize) -> Vec<ExpandedLine> {
    let mut lines = Vec::new();

    lines.push(ExpandedLine {
        text: truncate_str(&q.session_display_name, max_line_width),
        kind: LineKind::Info,
    });

    for (i, qi) in q.questions.iter().enumerate() {
        if i > 0 {
            lines.push(ExpandedLine {
                text: String::new(),
                kind: LineKind::Separator,
            });
        }
        lines.push(ExpandedLine {
            text: truncate_str(&format!("Q: {}", qi.question), max_line_width),
            kind: LineKind::Normal,
        });
        for (j, opt) in qi.options.iter().enumerate() {
            lines.push(ExpandedLine {
                text: truncate_str(&format!("  {}. {}", j + 1, opt.label), max_line_width),
                kind: LineKind::DiffContext,
            });
            if !opt.description.is_empty() {
                lines.push(ExpandedLine {
                    text: truncate_str(&format!("     {}", opt.description), max_line_width),
                    kind: LineKind::DiffContext,
                });
            }
        }
        if qi.custom.unwrap_or(true) {
            lines.push(ExpandedLine {
                text: truncate_str(
                    &format!("  {}. (type your own answer)", qi.options.len() + 1),
                    max_line_width,
                ),
                kind: LineKind::DiffContext,
            });
        }
    }

    lines
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
        // Separator is handled by the early return above; render as default if reached.
        LineKind::Separator => queue!(stdout, ResetColor)?,
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
        // Raw mode: dump the original JSON blob as-is.
        let json = serde_json::to_string_pretty(&approval.tool_input).unwrap_or_default();
        for line in json.lines() {
            lines.push(ExpandedLine {
                text: truncate_str(line, max_line_width),
                kind: LineKind::Normal,
            });
        }
    } else {
        // Parse into a typed ToolCall to avoid manual JSON introspection.
        let tool_call =
            ToolCall::try_from((approval.tool.clone(), approval.tool_input.clone())).ok();

        match tool_call.as_ref().map(|tc| tc.kind()) {
            Some(ToolCallKind::Write { path, content }) => {
                build_write_diff_lines(path, content.as_deref(), max_line_width, &mut lines);
            }
            _ => {
                // Generic key-value display of the raw JSON for all other tools.
                let input_str = format_tool_input(&approval.tool_input);
                for line in input_str.lines() {
                    lines.push(ExpandedLine {
                        text: truncate_str(line, max_line_width),
                        kind: LineKind::Normal,
                    });
                }
            }
        }
    }

    if let Some(extra) = &approval.context.extra {
        let text = match extra {
            protocol::ExtraContext::DippyReason { dippy_reason } => {
                format!("Dippy: {dippy_reason}")
            }
            protocol::ExtraContext::Diff { diff } => format!("Context: {diff}"),
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

/// Build diff lines for a Write tool request using typed fields from `ToolCallKind::Write`.
fn build_write_diff_lines(
    path: &str,
    new_contents: Option<&str>,
    max_line_width: usize,
    lines: &mut Vec<ExpandedLine>,
) {
    use similar::{ChangeTag, TextDiff};

    let new_contents = new_contents.unwrap_or("");

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

pub(crate) mod ui;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::Event;
use uuid::Uuid;

use crate::client::{Approval, Client, Question};
use protocol::Secret;
use ui::{Action, AnswerQuestionResult, DenyInputResult, Mode, ReviewItem, Ui};

pub async fn run(server: String, token: Option<String>, poll_interval: u64) -> anyhow::Result<()> {
    let server = server.trim_end_matches('/').to_string();
    let client = Client::new(server.clone(), token.map(Secret::new));

    // Health check before entering TUI
    client
        .health_check()
        .await
        .map_err(|e| anyhow::anyhow!("Cannot connect to server: {e}"))?;

    let mut ui = Ui::new(&server);
    ui.enter()?;

    let result = event_loop(&client, &mut ui, poll_interval).await;

    ui.leave()?;
    result
}

async fn event_loop(client: &Client, ui: &mut Ui, poll_interval_secs: u64) -> anyhow::Result<()> {
    let mut items: Vec<ReviewItem> = Vec::new();
    let mut known_approval_ids: HashSet<Uuid> = HashSet::new();
    let mut known_question_ids: HashSet<Uuid> = HashSet::new();
    let mut scroll_offsets: HashMap<Uuid, usize> = HashMap::new();
    let mut last_poll = Instant::now() - Duration::from_secs(poll_interval_secs + 1);
    let poll_interval = Duration::from_secs(poll_interval_secs);

    loop {
        // Poll server if interval elapsed
        if last_poll.elapsed() >= poll_interval {
            let (approvals_result, questions_result) = tokio::join!(
                client.list_pending(),
                client.list_pending_questions()
            );

            match (approvals_result, questions_result) {
                (Ok(new_approvals), Ok(new_questions)) => {
                    merge_items(
                        &mut items,
                        &mut known_approval_ids,
                        &mut known_question_ids,
                        new_approvals,
                        new_questions,
                    );
                    ui.clamp_selection(items.len());
                    ui.status_message = None;
                }
                (Err(e), _) | (_, Err(e)) => {
                    ui.status_message = Some(format!("Poll error: {e}"));
                }
            }
            last_poll = Instant::now();
        }

        // Sync per-item scroll offset before rendering
        if let Some(item) = items.get(ui.selected) {
            ui.scroll_offset = scroll_offsets.get(&item.id()).copied().unwrap_or(0);
        }

        // Render
        ui.render(&items)?;

        // Save back the (possibly clamped) scroll offset
        if let Some(item) = items.get(ui.selected) {
            scroll_offsets.insert(item.id(), ui.scroll_offset);
        }

        // Wait for event (short timeout so we re-poll regularly)
        let timeout = poll_interval
            .checked_sub(last_poll.elapsed())
            .unwrap_or(Duration::from_millis(100));
        let event = ui::poll_event(timeout.min(Duration::from_millis(500)))?;

        let Some(Event::Key(key)) = event else {
            continue;
        };

        // Ignore key release events (crossterm sends both press and release on some platforms)
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }

        match &ui.mode {
            Mode::Normal => {
                let action = ui.handle_normal_key(key, &items);
                match action {
                    Action::Approve(idx) => {
                        if let Some(ReviewItem::Approval(approval)) = items.get(idx) {
                            let id = approval.id;
                            match client.approve(id, None).await {
                                Ok(()) => {
                                    remove_item(&mut items, &mut known_approval_ids, &mut known_question_ids, id);
                                    ui.clamp_selection(items.len());
                                    last_poll = Instant::now() - poll_interval;
                                }
                                Err(e) => {
                                    ui.status_message = Some(format!("Approve failed: {e}"));
                                }
                            }
                        }
                    }
                    Action::StartDeny(idx) => {
                        ui.mode = Mode::DenyInput {
                            index: idx,
                            buffer: String::new(),
                        };
                    }
                    Action::StartAnswer(idx) => {
                        if let Some(ReviewItem::Question(q)) = items.get(idx) {
                            ui.mode = Mode::AnswerQuestion {
                                index: idx,
                                question: q.clone(),
                                // For each sub-question, track which option is selected (single-choice).
                                // We'll build a per-question selected index.
                                q_idx: 0,
                                selections: vec![None; q.questions.len()],
                                custom_inputs: vec![String::new(); q.questions.len()],
                            };
                        }
                    }
                    Action::RejectQuestion(idx) => {
                        if let Some(ReviewItem::Question(q)) = items.get(idx) {
                            let id = q.id;
                            match client.reject_question(id).await {
                                Ok(()) => {
                                    remove_item(&mut items, &mut known_approval_ids, &mut known_question_ids, id);
                                    ui.clamp_selection(items.len());
                                    last_poll = Instant::now() - poll_interval;
                                }
                                Err(e) => {
                                    ui.status_message = Some(format!("Reject failed: {e}"));
                                }
                            }
                        }
                    }
                    Action::ToggleRaw => {
                        ui.show_raw = !ui.show_raw;
                        if let Some(item) = items.get(ui.selected) {
                            scroll_offsets.insert(item.id(), 0);
                        }
                    }
                    Action::LineDown => {
                        if let Some(item) = items.get(ui.selected) {
                            let offset = scroll_offsets.get(&item.id()).copied().unwrap_or(0);
                            let max_offset = ui
                                .last_content_lines
                                .saturating_sub(ui.last_visible_rows.max(1));
                            if offset < max_offset {
                                scroll_offsets.insert(item.id(), offset + 1);
                            } else if items.len() > 1 {
                                ui.selected = (ui.selected + 1) % items.len();
                            }
                        }
                    }
                    Action::LineUp => {
                        if let Some(item) = items.get(ui.selected) {
                            let offset = scroll_offsets.get(&item.id()).copied().unwrap_or(0);
                            if offset > 0 {
                                scroll_offsets.insert(item.id(), offset - 1);
                            } else if items.len() > 1 {
                                ui.selected = (ui.selected + items.len() - 1) % items.len();
                                if let Some(prev) = items.get(ui.selected) {
                                    scroll_offsets.insert(prev.id(), usize::MAX);
                                }
                            }
                        }
                    }
                    Action::ScrollDown => {
                        if let Some(item) = items.get(ui.selected) {
                            let half_page = ui.last_visible_rows / 2;
                            let offset = scroll_offsets.get(&item.id()).copied().unwrap_or(0);
                            let max_offset = ui
                                .last_content_lines
                                .saturating_sub(ui.last_visible_rows.max(1));
                            scroll_offsets.insert(item.id(), (offset + half_page).min(max_offset));
                        }
                    }
                    Action::ScrollUp => {
                        if let Some(item) = items.get(ui.selected) {
                            let half_page = ui.last_visible_rows / 2;
                            let offset = scroll_offsets.get(&item.id()).copied().unwrap_or(0);
                            scroll_offsets.insert(item.id(), offset.saturating_sub(half_page));
                        }
                    }
                    Action::Next => {
                        if !items.is_empty() {
                            ui.selected = (ui.selected + 1) % items.len();
                        }
                    }
                    Action::Prev => {
                        if !items.is_empty() {
                            ui.selected = (ui.selected + items.len() - 1) % items.len();
                            if let Some(prev) = items.get(ui.selected) {
                                scroll_offsets.insert(prev.id(), usize::MAX);
                            }
                        }
                    }
                    Action::Quit => return Ok(()),
                    Action::None => {}
                }
            }
            Mode::DenyInput { .. } => {
                let result = ui.handle_deny_key(key);
                match result {
                    DenyInputResult::Confirm { index, reason } => {
                        ui.mode = Mode::Normal;
                        if let Some(ReviewItem::Approval(approval)) = items.get(index) {
                            let id = approval.id;
                            let reason = if reason.is_empty() { None } else { Some(reason) };
                            match client.deny(id, reason).await {
                                Ok(()) => {
                                    remove_item(&mut items, &mut known_approval_ids, &mut known_question_ids, id);
                                    ui.clamp_selection(items.len());
                                    last_poll = Instant::now() - poll_interval;
                                }
                                Err(e) => {
                                    ui.status_message = Some(format!("Deny failed: {e}"));
                                }
                            }
                        }
                    }
                    DenyInputResult::Cancel => {
                        ui.mode = Mode::Normal;
                    }
                    DenyInputResult::Continue => {}
                }
            }
            Mode::AnswerQuestion { .. } => {
                let result = ui.handle_answer_key(key);
                match result {
                    AnswerQuestionResult::Submit { id, answers } => {
                        ui.mode = Mode::Normal;
                        match client.answer_question(id, answers).await {
                            Ok(()) => {
                                remove_item(&mut items, &mut known_approval_ids, &mut known_question_ids, id);
                                ui.clamp_selection(items.len());
                                last_poll = Instant::now() - poll_interval;
                            }
                            Err(e) => {
                                ui.status_message = Some(format!("Answer failed: {e}"));
                            }
                        }
                    }
                    AnswerQuestionResult::Reject { id } => {
                        ui.mode = Mode::Normal;
                        match client.reject_question(id).await {
                            Ok(()) => {
                                remove_item(&mut items, &mut known_approval_ids, &mut known_question_ids, id);
                                ui.clamp_selection(items.len());
                                last_poll = Instant::now() - poll_interval;
                            }
                            Err(e) => {
                                ui.status_message = Some(format!("Reject failed: {e}"));
                            }
                        }
                    }
                    AnswerQuestionResult::Cancel => {
                        ui.mode = Mode::Normal;
                    }
                    AnswerQuestionResult::Continue => {}
                }
            }
        }
    }
}

/// Merge fresh approvals and questions into the items list, preserving order
/// (questions interleaved with approvals, sorted by created_at).
fn merge_items(
    items: &mut Vec<ReviewItem>,
    known_approval_ids: &mut HashSet<Uuid>,
    known_question_ids: &mut HashSet<Uuid>,
    new_approvals: Vec<Approval>,
    new_questions: Vec<Question>,
) {
    let new_a_ids: HashSet<Uuid> = new_approvals.iter().map(|a| a.id).collect();
    let new_q_ids: HashSet<Uuid> = new_questions.iter().map(|q| q.id).collect();

    // Remove items that are no longer pending
    items.retain(|item| match item {
        ReviewItem::Approval(a) => new_a_ids.contains(&a.id),
        ReviewItem::Question(q) => new_q_ids.contains(&q.id),
    });

    // Add new arrivals
    for a in new_approvals {
        if !known_approval_ids.contains(&a.id) {
            items.push(ReviewItem::Approval(a));
        }
    }
    for q in new_questions {
        if !known_question_ids.contains(&q.id) {
            items.push(ReviewItem::Question(q));
        }
    }

    // Sort by created_at (oldest first)
    items.sort_by_key(|item| item.created_at());

    // Rebuild known id sets
    *known_approval_ids = items
        .iter()
        .filter_map(|i| if let ReviewItem::Approval(a) = i { Some(a.id) } else { None })
        .collect();
    *known_question_ids = items
        .iter()
        .filter_map(|i| if let ReviewItem::Question(q) = i { Some(q.id) } else { None })
        .collect();
}

fn remove_item(
    items: &mut Vec<ReviewItem>,
    known_approval_ids: &mut HashSet<Uuid>,
    known_question_ids: &mut HashSet<Uuid>,
    id: Uuid,
) {
    items.retain(|item| item.id() != id);
    known_approval_ids.remove(&id);
    known_question_ids.remove(&id);
}

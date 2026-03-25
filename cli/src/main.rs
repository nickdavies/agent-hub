mod client;
mod ui;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::Event;
use uuid::Uuid;

use client::{Approval, Client};
use ui::{Action, DenyInputResult, Mode, Ui};

#[derive(Parser)]
#[command(
    name = "claude-review",
    version,
    about = "Interactive CLI for reviewing Claude Code approvals"
)]
struct Args {
    /// Server URL
    #[arg(
        long,
        env = "CLAUDE_NOTIFY_SERVER",
        default_value = "http://localhost:8080"
    )]
    server: String,

    /// Bearer token for authenticated servers (optional for no-auth mode)
    #[arg(long, env = "CLAUDE_NOTIFY_TOKEN")]
    token: Option<String>,

    /// Poll interval in seconds
    #[arg(long, default_value = "2")]
    poll_interval: u64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let server = args.server.trim_end_matches('/').to_string();
    let client = Client::new(server.clone(), args.token);

    // Health check before entering TUI
    client
        .health_check()
        .await
        .map_err(|e| anyhow::anyhow!("Cannot connect to server: {e}"))?;

    let mut ui = Ui::new(&server);
    ui.enter()?;

    let result = event_loop(&client, &mut ui, args.poll_interval).await;

    ui.leave()?;
    result
}

async fn event_loop(client: &Client, ui: &mut Ui, poll_interval_secs: u64) -> anyhow::Result<()> {
    let mut approvals: Vec<Approval> = Vec::new();
    let mut known_ids: HashSet<Uuid> = HashSet::new();
    let mut scroll_offsets: HashMap<Uuid, usize> = HashMap::new();
    let mut last_poll = Instant::now() - Duration::from_secs(poll_interval_secs + 1);
    let poll_interval = Duration::from_secs(poll_interval_secs);

    loop {
        // Poll server if interval elapsed
        if last_poll.elapsed() >= poll_interval {
            match client.list_pending().await {
                Ok(new_approvals) => {
                    let new_ids: HashSet<Uuid> = new_approvals.iter().map(|a| a.id).collect();

                    // Detect removals (resolved externally)
                    let removed: Vec<Uuid> = known_ids.difference(&new_ids).copied().collect();
                    if !removed.is_empty() {
                        approvals.retain(|a| !removed.contains(&a.id));
                    }

                    // Detect additions
                    for a in new_approvals {
                        if !known_ids.contains(&a.id) {
                            approvals.push(a);
                        }
                    }

                    // Sort by created_at (oldest first)
                    approvals.sort_by_key(|a| a.created_at);
                    known_ids = new_ids;

                    ui.clamp_selection(approvals.len());
                    ui.status_message = None;
                }
                Err(e) => {
                    ui.status_message = Some(format!("Poll error: {e}"));
                }
            }
            last_poll = Instant::now();
        }

        // Sync per-approval scroll offset before rendering
        if let Some(a) = approvals.get(ui.selected) {
            ui.scroll_offset = scroll_offsets.get(&a.id).copied().unwrap_or(0);
        }

        // Render
        ui.render(&approvals)?;

        // Save back the (possibly clamped) scroll offset
        if let Some(a) = approvals.get(ui.selected) {
            scroll_offsets.insert(a.id, ui.scroll_offset);
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
                let action = ui.handle_normal_key(key, approvals.len());
                match action {
                    Action::Approve(idx) => {
                        if let Some(approval) = approvals.get(idx) {
                            let id = approval.id;
                            match client.approve(id, None).await {
                                Ok(()) => {
                                    remove_approval(&mut approvals, &mut known_ids, id);
                                    ui.clamp_selection(approvals.len());
                                    // Force re-poll to sync state
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
                    Action::ToggleRaw => {
                        ui.show_raw = !ui.show_raw;
                        if let Some(a) = approvals.get(ui.selected) {
                            scroll_offsets.insert(a.id, 0);
                        }
                    }
                    Action::LineDown => {
                        if let Some(a) = approvals.get(ui.selected) {
                            let offset = scroll_offsets.get(&a.id).copied().unwrap_or(0);
                            let max_offset = ui
                                .last_content_lines
                                .saturating_sub(ui.last_visible_rows.max(1));
                            if offset < max_offset {
                                scroll_offsets.insert(a.id, offset + 1);
                            } else if approvals.len() > 1 {
                                ui.selected = (ui.selected + 1) % approvals.len();
                            }
                        }
                    }
                    Action::LineUp => {
                        if let Some(a) = approvals.get(ui.selected) {
                            let offset = scroll_offsets.get(&a.id).copied().unwrap_or(0);
                            if offset > 0 {
                                scroll_offsets.insert(a.id, offset - 1);
                            } else if approvals.len() > 1 {
                                ui.selected =
                                    (ui.selected + approvals.len() - 1) % approvals.len();
                                if let Some(prev) = approvals.get(ui.selected) {
                                    scroll_offsets.insert(prev.id, usize::MAX);
                                }
                            }
                        }
                    }
                    Action::ScrollDown => {
                        if let Some(a) = approvals.get(ui.selected) {
                            let half_page = ui.last_visible_rows / 2;
                            let offset = scroll_offsets.get(&a.id).copied().unwrap_or(0);
                            let max_offset = ui
                                .last_content_lines
                                .saturating_sub(ui.last_visible_rows.max(1));
                            scroll_offsets
                                .insert(a.id, (offset + half_page).min(max_offset));
                        }
                    }
                    Action::ScrollUp => {
                        if let Some(a) = approvals.get(ui.selected) {
                            let half_page = ui.last_visible_rows / 2;
                            let offset = scroll_offsets.get(&a.id).copied().unwrap_or(0);
                            scroll_offsets.insert(a.id, offset.saturating_sub(half_page));
                        }
                    }
                    Action::Next => {
                        if !approvals.is_empty() {
                            ui.selected = (ui.selected + 1) % approvals.len();
                        }
                    }
                    Action::Prev => {
                        if !approvals.is_empty() {
                            ui.selected =
                                (ui.selected + approvals.len() - 1) % approvals.len();
                            if let Some(prev) = approvals.get(ui.selected) {
                                scroll_offsets.insert(prev.id, usize::MAX);
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
                        if let Some(approval) = approvals.get(index) {
                            let id = approval.id;
                            let reason = if reason.is_empty() {
                                None
                            } else {
                                Some(reason)
                            };
                            match client.deny(id, reason).await {
                                Ok(()) => {
                                    remove_approval(&mut approvals, &mut known_ids, id);
                                    ui.clamp_selection(approvals.len());
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
        }
    }
}

fn remove_approval(approvals: &mut Vec<Approval>, known_ids: &mut HashSet<Uuid>, id: Uuid) {
    approvals.retain(|a| a.id != id);
    known_ids.remove(&id);
}

//! Chat screen renderer (block-aware).
//!
//! Goal-144 redraws the messages panel using
//! [`crate::tui::ui::transcript::render_blocks`] (one block per logical
//! transcript entry, separated by blank lines) and replaces the old
//! single-line status bar with the rich
//! [`crate::tui::ui::status::render`] formatter.
//!
//! Goal-145 swaps the single-line input footer for the multi-mode
//! [`crate::tui::ui::input`] renderer (input box + dynamic height + footer
//! hint) and lets the terminal native cursor land on the actual edit
//! position.
//!
//! Goal-167 adds a compact task-list panel between the messages area and
//! the status bar when `current_todos` is non-empty.
//!
//! While a turn is running the spinner from
//! [`crate::tui::ui::spinner::format_line`] is appended after the last
//! block.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tools::todo::TodoStatus;
use crate::tui::app::App;
use crate::tui::ui::{command_menu, input, modal, spinner, status, transcript};

/// Height of the todo panel (border + one row per item, capped at 6 items).
fn todo_panel_height(app: &App) -> u16 {
    if app.current_todos.is_empty() {
        0
    } else {
        // 2 for the border + 1 per item (capped so it doesn't take over)
        (app.current_todos.len().min(6) as u16) + 2
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let input_total = input::total_height(app);
    let todo_height = todo_panel_height(app);
    // Fix-E: show a 1-row approval banner when a plan is awaiting the
    // user's decision. The banner replaces the floating modal and keeps
    // the full transcript visible.
    // Goal-202: also show 1-row banner when plan-mode entry request is pending.
    let plan_banner_height: u16 = if app.plan_awaiting_approval || app.plan_mode_request_pending {
        1
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                     // messages
            Constraint::Length(todo_height),        // Goal-167: task list (0 when empty)
            Constraint::Length(1),                  // status bar
            Constraint::Length(plan_banner_height), // Fix-E: plan approval banner
            Constraint::Length(input_total),        // input + footer hint
        ])
        .split(frame.area());

    // Messages panel.
    let mut lines = transcript::render_blocks(&app.blocks, &app.usage, app.theme);
    if app.turn.running {
        let elapsed = app
            .turn
            .started_at
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![Span::styled(
            spinner::format_line(app.spinner_frame, app.turn.spinner_verb, elapsed),
            Style::default().fg(Color::Yellow),
        )]));
    }

    let messages_area = chunks[0];
    let todo_area = chunks[1];
    let inner_width = messages_area.width.saturating_sub(2); // borders
    let visible_rows = messages_area.height.saturating_sub(2);

    // Compute the *visual* (post-wrap) row count for proper scroll
    // capping. Counting `lines.len()` (logical rows) under-counts when
    // long messages wrap, which silently caps `scroll_offset` and
    // prevents scrolling all the way to the top of long transcripts.
    //
    // ratatui 0.29's `Paragraph::line_count` is unstable, so we
    // approximate with `Line::width()` (public, unicode-aware) divided
    // by the inner width and rounded up. Empty lines still count as 1.
    let total_rows: u16 = if inner_width == 0 {
        lines.len() as u16
    } else {
        let w = inner_width as usize;
        let sum: usize = lines
            .iter()
            .map(|l| {
                let lw = l.width();
                if lw == 0 {
                    1
                } else {
                    lw.div_ceil(w)
                }
            })
            .sum();
        sum.try_into().unwrap_or(u16::MAX)
    };
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let effective_scroll = max_scroll.saturating_sub(app.scroll_offset.min(max_scroll));

    let messages_widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));
    frame.render_widget(messages_widget, messages_area);

    // Goal-167: task-list panel (only rendered when non-empty).
    if !app.current_todos.is_empty() {
        render_todo_panel(frame, todo_area, app);
    }

    // Status bar.
    status::render(frame, chunks[2], app);

    // Fix-E: plan approval banner (1 row, visible only when plan_awaiting_approval).
    // Goal-202: also shown when plan_mode_request_pending.
    if app.plan_awaiting_approval {
        render_plan_approval_banner(frame, chunks[3], app);
    } else if app.plan_mode_request_pending {
        render_plan_mode_request_banner(frame, chunks[3]);
    }

    // Input panel + footer hint (chunks[4] when banner present, [3] otherwise).
    // The layout always reserves the slot; when plan_banner_height=0 the
    // area has zero height and input renders normally at [4].
    input::render(frame, chunks[4], app);

    // Goal-146: floating slash-command popup, drawn after the input
    // box so it overlays the messages panel.
    command_menu::render(frame, chunks[4], app);

    // Goal-158: @file completion popup.
    command_menu::render_atfile(frame, chunks[4], app);

    // Goal-160: Ctrl+R history-search popup.
    command_menu::render_history_search(frame, chunks[4], app);

    // Goal-161: permission-request modal (top layer — covers everything).
    command_menu::render_permission_modal(frame, app);

    // Goal-146: modals are last so they cover everything else.
    if !app.modals.is_empty() {
        modal::render(frame, app);
    }
}

/// Render the compact task-list panel.
///
/// Shows up to 6 items with ✓/◉/○/✗ status icons. Items beyond the first
/// 6 are silently truncated (the agent should keep lists short).
fn render_todo_panel(frame: &mut Frame, area: Rect, app: &App) {
    let completed = app
        .current_todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let total = app.current_todos.len();
    let title = format!(" Tasks ({completed}/{total} done) ");

    let items: Vec<Line> = app
        .current_todos
        .iter()
        .take(6)
        .map(|item| {
            let (icon, style) = match item.status {
                TodoStatus::Completed => ("✓", Style::default().fg(Color::Green)),
                TodoStatus::InProgress => ("◉", Style::default().fg(Color::Yellow)),
                TodoStatus::Pending => ("○", Style::default().fg(Color::DarkGray)),
                TodoStatus::Cancelled => ("✗", Style::default().fg(Color::DarkGray)),
            };
            let label = item
                .active_form
                .as_deref()
                .filter(|_| item.status == TodoStatus::InProgress)
                .unwrap_or(&item.content);
            Line::from(vec![
                Span::styled(format!(" {icon} "), style),
                Span::styled(label.to_string(), style),
            ])
        })
        .collect();

    let widget = Paragraph::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

/// Fix-E: render a 1-row plan approval banner between the status bar
/// and the input box. Visible only while `plan_awaiting_approval` is set.
///
/// ```text
/// ⚡ Plan awaiting approval — [y] Approve  [n] Reject  [e] Edit
/// ```
fn render_plan_approval_banner(frame: &mut Frame, area: Rect, _app: &App) {
    use ratatui::style::Modifier;
    let line = Line::from(vec![
        Span::styled(
            " ⚡ Plan awaiting approval — ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "[y/Enter]",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Approve  ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::styled(
            "[n/Esc]",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Reject  ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::styled(
            "[e]",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Edit ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
    ]);
    let widget = Paragraph::new(line);
    frame.render_widget(widget, area);
}

/// Goal-202: render a 1-row plan-mode request banner between the status bar
/// and the input box. Visible while `plan_mode_request_pending` is set.
///
/// ```text
///  ⓘ Plan mode request — [y/Enter] Allow   [n/Esc] Skip
/// ```
fn render_plan_mode_request_banner(frame: &mut Frame, area: Rect) {
    use ratatui::style::Modifier;
    let line = Line::from(vec![
        Span::styled(
            " ⓘ Plan mode request — ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "[y/Enter]",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Allow  ",
            Style::default().fg(Color::Black).bg(Color::Blue),
        ),
        Span::styled(
            "[n/Esc]",
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Skip — execute directly ",
            Style::default().fg(Color::White).bg(Color::Blue),
        ),
    ]);
    let widget = Paragraph::new(line)
        .style(Style::default().bg(Color::Blue))
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

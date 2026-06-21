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
    // The bottom panel slot (below the input box) expands when a slash-command,
    // @file, or history-search panel is active, pushing the input upward.
    // When no interactive panel is open, the height is 0 and the slot is invisible.
    let panel_h = command_menu::panel_height(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                     // 0: messages
            Constraint::Length(todo_height),        // 1: Goal-167 task list (0 when empty)
            Constraint::Length(1),                  // 2: status bar
            Constraint::Length(plan_banner_height), // 3: Fix-E plan approval banner
            Constraint::Length(input_total),        // 4: input + footer hint
            Constraint::Length(panel_h),            // 5: interactive panel below input
        ])
        .split(frame.area());

    // Messages panel: render the full transcript top-anchored so content
    // grows downward from the top of the screen (full-screen UX), with the
    // input box pinned at the bottom. When there is nothing to show yet we
    // draw a centred startup splash (logo + hints) instead.
    let messages_area = chunks[0];
    let todo_area = chunks[1];

    if app.blocks.is_empty() && !app.turn.running {
        render_empty_state(frame, messages_area, app);
    } else {
        let mut lines: Vec<Line<'static>> =
            transcript::render_blocks(&app.blocks, &app.usage, app.theme, messages_area.width);

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
        // Keep one blank row between the last content line and the status bar
        // so the output doesn't visually collide with it.
        lines.push(Line::raw(""));

        // The messages panel no longer wraps in a bordered `Block`, so the
        // area is the chunk itself — no border rows / columns to subtract.
        let inner_width = messages_area.width;
        let visible = messages_area.height as usize;

        // Pre-wrap every logical line into physical rows at the exact panel
        // width, then window those rows ourselves in `usize`. This replaces
        // the previous `Paragraph::scroll` + estimated-row-count scheme, whose
        // char-width row estimate drifted from ratatui's word-aware wrapping
        // (producing inexact scroll positions and rows that could never be
        // scrolled into view) and whose `as u16` scroll cast could overflow on
        // very long transcripts. Exact windowing means both ends are always
        // reachable.
        let physical = transcript::wrap_lines_to_width(&lines, inner_width);
        let total_rows = physical.len();
        let max_scroll = total_rows.saturating_sub(visible);
        // `scroll_offset` counts rows from the bottom. Capping it at
        // `max_scroll` keeps `scroll_offset == 0` stuck to the bottom (newest
        // content visible) while letting a large offset scroll all the way to
        // the first row. The transcript is top-anchored, so a short
        // conversation fills from the top with blank space below.
        let capped = app.scroll_offset.min(max_scroll);
        let start = max_scroll - capped;
        let end = (start + visible).min(total_rows);
        let window: Vec<Line<'static>> = physical[start..end].to_vec();

        // Rows are already wrapped to `inner_width`, so render without
        // additional wrapping or scroll offset.
        let messages_widget = Paragraph::new(window);
        frame.render_widget(messages_widget, messages_area);
    }

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

    // Input panel + footer hint.
    input::render(frame, chunks[4], app);

    // Goal-146/158/160: interactive panel below the input box (chunks[5]).
    // When no panel is active, panel_h == 0 and the slot has zero height.
    // Active panels push the input box upward via Layout shrinking messages.
    command_menu::render_panel(frame, chunks[5], app);

    // Goal-161: permission-request modal (centred overlay — covers everything).
    command_menu::render_permission_modal(frame, app);

    // Goal-146: modals are last so they cover everything else.
    if !app.modals.is_empty() {
        modal::render(frame, app);
    }
}

/// Render the full-screen startup splash shown while the transcript is
/// empty: a centred wordmark logo, version + model, and a short hint row.
///
/// This replaces the old "logo + recent sessions glued above the input box"
/// banner. Recent sessions now live behind `/resume`, keeping the empty
/// state clean and the focus on the input.
fn render_empty_state(frame: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::Modifier;

    let orange_bold = Style::default()
        .fg(Color::Rgb(205, 100, 50))
        .add_modifier(Modifier::BOLD);
    let orange = Style::default().fg(Color::Rgb(205, 100, 50));
    let gray = Style::default().fg(Color::Rgb(150, 150, 150));
    let dim = Style::default().fg(Color::Rgb(110, 110, 110));

    let version = env!("CARGO_PKG_VERSION");
    let model = app.model_name.clone();

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("┬─┐┌─┐┌─┐┬ ┬┬─┐┌─┐┬┬  ┬┌─┐", orange_bold)),
        Line::from(Span::styled("├┬┘├┤ │  │ │├┬┘└─┐│└┐┌┘├┤ ", orange_bold)),
        Line::from(Span::styled("┴└─└─┘└─┘└─┘┴└─└─┘┴ └┘ └─┘", orange)),
        Line::raw(""),
        Line::from(Span::styled(format!("v{version}  ·  {model}"), gray)),
        Line::raw(""),
        Line::from(Span::styled("Type a message to start", dim)),
        Line::from(Span::styled(
            "/resume to continue a session  ·  /help for commands",
            dim,
        )),
    ];

    // Vertically centre by padding the top with blank rows.
    let content_h = lines.len() as u16;
    if area.height > content_h {
        let pad = (area.height - content_h) / 2;
        let mut padded: Vec<Line<'static>> = (0..pad).map(|_| Line::raw("")).collect();
        padded.append(&mut lines);
        lines = padded;
    }

    let widget = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(widget, area);
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

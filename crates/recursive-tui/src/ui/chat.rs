//! Chat screen renderer (block-aware).
//!
//! Goal-144 redraws the messages panel using
//! [`crate::ui::transcript::render_blocks`] (one block per logical
//! transcript entry, separated by blank lines) and replaces the old
//! single-line status bar with the rich
//! [`crate::ui::status::render`] formatter.
//!
//! Goal-145 swaps the single-line input footer for the multi-mode
//! [`crate::ui::input`] renderer (input box + dynamic height + footer
//! hint) and lets the terminal native cursor land on the actual edit
//! position.
//!
//! Goal-167 adds a compact task-list panel between the messages area and
//! the status bar when `current_todos` is non-empty.
//!
//! While a turn is running the spinner from
//! [`crate::ui::spinner::format_line`] is appended after the last
//! block.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;
use crate::ui::{command_menu, input, modal, spinner, status, transcript};
use recursive::tools::todo::TodoStatus;

/// Height of the todo panel (border + one row per item).
///
/// Goal-384: the panel grows with the list up to ~1/3 of the screen so
/// the windowing logic has room to show context around the in-progress
/// task, but never shrinks below the old 6-item default for short lists.
/// `screen_height` is the full terminal height (`frame.area().height`),
/// used only to derive the cap.
fn todo_panel_height(app: &App, screen_height: u16) -> u16 {
    if app.current_todos.is_empty() {
        return 0;
    }
    // Cap at ~1/3 of the screen so the transcript keeps the majority of
    // vertical space. `max(3)` guards a tiny terminal; `.max(6)` keeps
    // the old default for lists that fit.
    let by_items = app.current_todos.len() as u16;
    let max_by_screen = screen_height.saturating_sub(8).max(3) / 3;
    let cap = max_by_screen.max(6);
    by_items.min(cap) + 2 // +2 for the border
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let input_total = input::total_height(app, frame.area().width);
    let todo_height = todo_panel_height(app, frame.area().height);
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
        // Goal-349: record the messages-panel size for the copy paths. The
        // mouse / yank handlers recompute the visible window via
        // `visible_physical_rows` and must use the same width + height this
        // paint used, or the copied text would drift from the screen.
        app.last_render_width = messages_area.width;
        app.last_render_height = messages_area.height;

        let mut window = visible_physical_rows(app, messages_area.width);

        // Goal-349: reverse-highlight the selected row range. Selection is
        // stored in visible-window coordinates, which exactly matches
        // `window`'s indexing, so no coordinate translation is needed.
        // Modifier::REVERSED inverts fg/bg regardless of the row's existing
        // colours — important because assistant markdown rows carry per-span
        // syntax colours we must not fight with a hard-coded background.
        if let Some((s, e)) = app.selection {
            use ratatui::style::Modifier;
            // Selection is stored as (anchor, cursor); normalise so the
            // highlight spans the inclusive range in either drag direction.
            let (s, e) = (s.min(e), s.max(e));
            let lo = s.min(window.len());
            let hi = (e + 1).min(window.len());
            for line in &mut window[lo..hi] {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
        }

        // Rows are already wrapped to the panel width, so render without
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

/// Goal-349: the flattened, width-wrapped physical rows currently visible in
/// the messages panel — exactly the `window` slice [`render`] paints.
///
/// This is the single source of truth for the row-windowing math (render
/// blocks → wrap to width → slice by `scroll_offset`). Both the render path
/// (which restyles the rows when a selection is active) and the mouse /
/// keyboard copy paths (which slice text out of the rows) call this, so the
/// selection text can never diverge from what is painted.
///
/// `width` is the messages-panel width in columns. The visible height and
/// scroll offset are read from [`App::last_render_height`] and
/// [`App::scroll_offset`] so the function reproduces the exact window of the
/// last render — the copy paths run between renders and must not re-derive
/// the width themselves.
pub fn visible_physical_rows(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> =
        transcript::render_blocks(&app.blocks, &app.usage, app.theme, width);

    if app.turn.running {
        let elapsed = app
            .turn
            .step_started_at
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

    let physical = transcript::wrap_lines_to_width(&lines, width);
    let total_rows = physical.len();
    let visible = app.last_render_height as usize;
    let max_scroll = total_rows.saturating_sub(visible);
    // `scroll_offset` counts rows from the bottom. Capping it at
    // `max_scroll` keeps `scroll_offset == 0` stuck to the bottom (newest
    // content visible) while letting a large offset scroll all the way to
    // the first row. The transcript is top-anchored, so a short
    // conversation fills from the top with blank space below.
    let capped = app.scroll_offset.min(max_scroll);
    let start = max_scroll - capped;
    let end = (start + visible).min(total_rows);
    physical[start..end].to_vec()
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
    let red = Style::default().fg(Color::Rgb(205, 80, 80));
    let red_bold = Style::default()
        .fg(Color::Rgb(205, 80, 80))
        .add_modifier(Modifier::BOLD);

    let version = env!("CARGO_PKG_VERSION");
    // When offline, the hardcoded `deepseek-v4-flash` model fallback is
    // misleading — show "no provider" instead so the user understands the
    // agent can't run. The status bar does the same for its model slot.
    let model = if app.offline_reason.is_some() {
        "no provider".to_string()
    } else {
        app.model_name.clone()
    };

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("┬─┐┌─┐┌─┐┬ ┬┬─┐┌─┐┬┬  ┬┌─┐", orange_bold)),
        Line::from(Span::styled("├┬┘├┤ │  │ │├┬┘└─┐│└┐┌┘├┤ ", orange_bold)),
        Line::from(Span::styled("┴└─└─┘└─┘└─┘┴└─└─┘┴ └┘ └─┘", orange)),
        Line::raw(""),
        Line::from(Span::styled(format!("v{version}  ·  {model}"), gray)),
        Line::raw(""),
    ];

    if app.offline_reason.is_some() {
        // No usable runtime was built (missing API key / preset). Show an
        // actionable setup hint in place of the "Type a message to start"
        // splash — the user's next step is to configure a provider outside
        // the TUI and restart, not to type a message.
        lines.push(Line::from(Span::styled(
            "Offline — no LLM provider configured.",
            red_bold,
        )));
        lines.push(Line::from(Span::styled(
            "The agent can't run until you set one up.",
            red,
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  1) Outside the TUI, run:  recursive init   (interactive wizard)",
            dim,
        )));
        lines.push(Line::from(Span::styled("  2) Or configure manually:", dim)));
        lines.push(Line::from(Span::styled(
            "       recursive config set provider.preset <id>",
            dim,
        )));
        lines.push(Line::from(Span::styled(
            "       recursive config set-secret <KEY_ENV> <KEY>",
            dim,
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  Then /exit (or Ctrl+C twice) and restart `cargo tui`.",
            dim,
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "/resume to continue a session  ·  /help for commands",
            dim,
        )));
    } else {
        lines.push(Line::from(Span::styled("Type a message to start", dim)));
        lines.push(Line::from(Span::styled(
            "/resume to continue a session  ·  /help for commands",
            dim,
        )));
    }

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

/// Compute the visible window `[start, end)` over a todo list of `total`
/// items, given the (optional) index of the in-progress item and the
/// number of content rows available inside the panel (`area.height - 2`
/// for the border).
///
/// Goal-384: guarantees the in-progress item stays visible when the list
/// is longer than the panel — the single most common "where did the agent
/// go?" confusion in long sessions. The in-progress item is centred in
/// the window (clamped to the list bounds) so the user sees context above
/// and below it. When there is no in-progress item, the window pins to
/// the tail (most recent activity), mirroring how the transcript
/// scrolls to the bottom.
///
/// Returns `(0, 0)` for an empty list or a zero-height panel.
fn todo_window(total: usize, anchor: Option<usize>, content_rows: usize) -> (usize, usize) {
    if total == 0 || content_rows == 0 {
        return (0, 0);
    }
    if total <= content_rows {
        return (0, total);
    }
    let start = match anchor {
        // Centre the anchor, then clamp so the window never overshoots
        // the start or end of the list.
        Some(idx) => {
            let ideal_start = idx.saturating_sub(content_rows / 2);
            ideal_start.min(total.saturating_sub(content_rows))
        }
        // No anchor → show the tail (most recent activity).
        None => total.saturating_sub(content_rows),
    };
    (start, (start + content_rows).min(total))
}

/// Render the task-list panel.
///
/// Goal-384: instead of truncating to the first 6 items, the panel
/// windows around the in-progress item so it is always visible. The
/// visible slice IS the window (no `Paragraph::scroll` needed — the
/// `Paragraph` starts at item `start`), and the title shows how many
/// items are scrolled off the top/bottom when the list is longer than
/// the panel.
fn render_todo_panel(frame: &mut Frame, area: Rect, app: &App) {
    let completed = app
        .current_todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let total = app.current_todos.len();

    // Content rows available inside the bordered panel (area.height - 2
    // for top + bottom border). The in-progress item is the row that
    // MUST stay visible; `todo_write` enforces at most one, so
    // `.position(...)` yields a single deterministic index (or None).
    let content_rows = area.height.saturating_sub(2) as usize;
    let anchor = app
        .current_todos
        .iter()
        .position(|t| t.status == TodoStatus::InProgress);
    let (start, end) = todo_window(total, anchor, content_rows);
    let hidden_top = start;
    let hidden_bottom = total.saturating_sub(end);

    let mut title = format!(" Tasks ({completed}/{total} done) ");
    if hidden_top > 0 || hidden_bottom > 0 {
        // Truncation indicator: `↑2` = 2 items scrolled off the top,
        // `↓3` = 3 off the bottom.
        title.push_str(&format!("↑{hidden_top} ↓{hidden_bottom} "));
    }

    let items: Vec<Line> = app.current_todos[start..end]
        .iter()
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

#[cfg(test)]
mod debt_tests {
    use super::*;
    use crate::model::TranscriptBlock;
    use crate::ui::modal::Modal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;
    use recursive::tools::todo::{TodoItem, TodoStatus};

    fn draw(app: &mut App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).expect("TestBackend infallible");
        term.draw(|fr| render(fr, app)).expect("draw infallible");
        term.backend().buffer().clone()
    }

    fn all_text(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf.cell((x, y)).expect("cell").symbol());
            }
            s.push('\n');
        }
        s
    }

    fn row_y_containing(buf: &Buffer, needle: &str) -> Option<u16> {
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf.cell((x, y)).expect("cell").symbol());
            }
            if row.contains(needle) {
                return Some(y);
            }
        }
        None
    }

    fn todo(content: &str, status: TodoStatus, active_form: Option<&str>) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            status,
            active_form: active_form.map(str::to_string),
        }
    }

    #[test]
    fn todo_panel_height_zero_when_empty() {
        // kills todo_panel_height -> 0 (29:5) via the empty-list branch.
        let app = App::new();
        assert_eq!(todo_panel_height(&app, 24), 0);
    }

    #[test]
    fn todo_panel_height_grows_with_items_up_to_screen_cap() {
        // Goal-384: the cap is now screen-relative (~1/3 of the screen,
        // never below the old 6-item default) instead of hardcoded at 6.
        // Tall screen (40 rows): 9 items fit (9+2=11 rows) because the
        // screen cap is 10. Short screen (24 rows): the cap is 6, so 9
        // items render only 6+2=8 rows. Together these pin the
        // `min(cap)` clamp and the `saturating_sub(8) / 3` screen cap.
        let mut app = App::new();

        // 1 item + 2 border, regardless of screen height.
        app.current_todos = vec![todo("a", TodoStatus::Pending, None)];
        assert_eq!(todo_panel_height(&app, 40), 3);

        // 6 items + 2 border on both screens (fits under every cap).
        app.current_todos = (0..6)
            .map(|i| todo(&format!("t{i}"), TodoStatus::Pending, None))
            .collect();
        assert_eq!(todo_panel_height(&app, 40), 8);
        assert_eq!(todo_panel_height(&app, 24), 8);

        // Tall screen: 9 items grow past the old 6-item cap → 11 rows.
        app.current_todos = (0..9)
            .map(|i| todo(&format!("t{i}"), TodoStatus::Pending, None))
            .collect();
        assert_eq!(todo_panel_height(&app, 40), 11);

        // Short screen: the 1/3-screen cap (6) kicks in → 8 rows.
        assert_eq!(todo_panel_height(&app, 24), 8);
    }

    #[test]
    fn render_plan_banner_on_approval_only() {
        // plan_awaiting_approval=true, plan_mode_request_pending=false.
        // orig: `true || false` -> banner height 1 -> banner visible.
        // mutant `&&`: `true && false` -> height 0 -> banner not drawn.
        // kills 45:65 `||`->`&&`.
        let mut app = App::new();
        app.plan_awaiting_approval = true;
        app.plan_mode_request_pending = false;
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains("Plan awaiting approval"),
            "expected plan approval banner text"
        );
    }

    #[test]
    fn render_plan_mode_banner_when_pending() {
        // kills render_plan_mode_request_banner -> () (309:5).
        let mut app = App::new();
        app.plan_mode_request_pending = true;
        app.plan_awaiting_approval = false;
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains("Plan mode request"),
            "expected plan mode request banner text"
        );
    }

    #[test]
    fn render_empty_state_centers_logo_vertically() {
        // blocks empty + turn not running -> render_empty_state. With a
        // tall terminal the logo is padded down by (mh-9)/2 rows. mutant
        // `/`->`%` pads by (mh-9)%2 (0 or 1) -> logo near the top.
        // kills 196:45 `/`->`%`.
        let mut app = App::new();
        let buf = draw(&mut app, 80, 60);
        let y = row_y_containing(&buf, "┬─┐").expect("logo row should be present");
        assert!(
            y >= 10,
            "logo should be vertically centred (y>=10), got y={y}"
        );
    }

    #[test]
    fn render_empty_state_shows_offline_setup_guidance() {
        // When no provider is configured, the empty state must surface an
        // actionable setup hint (recursive init) and show "no provider"
        // instead of the misleading hardcoded model fallback. Pins the
        // offline branch of render_empty_state.
        let mut app = App::new();
        app.model_name = "deepseek-v4-flash".to_string();
        app.offline_reason = Some("No LLM provider configured.".to_string());
        let buf = draw(&mut app, 100, 30);
        let text = all_text(&buf);
        assert!(
            text.contains("Offline"),
            "expected 'Offline' heading, got:\n{text}"
        );
        assert!(
            text.contains("recursive init"),
            "expected wizard hint, got:\n{text}"
        );
        assert!(
            text.contains("no provider"),
            "expected 'no provider' model label, got:\n{text}"
        );
        assert!(
            !text.contains("Type a message to start"),
            "online splash hint should be hidden when offline, got:\n{text}"
        );
        // /resume + /help should still be advertised so the user can
        // discover commands even while offline.
        assert!(text.contains("/resume") && text.contains("/help"));
    }

    #[test]
    fn render_todo_panel_visible_when_todos_present() {
        // turn.running=true so the empty-state splash is NOT shown; the
        // todo panel renders below the messages area.
        // kills render_todo_panel -> () (211:5).
        let mut app = App::new();
        app.current_todos = vec![todo("Task one", TodoStatus::Pending, None)];
        app.turn.running = true;
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains("Tasks"),
            "expected todo panel title 'Tasks'"
        );
    }

    #[test]
    fn render_todo_panel_counts_completed_in_title() {
        // 3 todos: 1 Completed, 2 Pending. orig title "1/3 done".
        // mutant `==`->`!=`: counts non-Completed -> "2/3 done".
        // kills 214:30 `==`->`!=`.
        let mut app = App::new();
        app.current_todos = vec![
            todo("done", TodoStatus::Completed, None),
            todo("p1", TodoStatus::Pending, None),
            todo("p2", TodoStatus::Pending, None),
        ];
        app.turn.running = true;
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains("1/3"),
            "expected '1/3' in todo title"
        );
    }

    #[test]
    fn render_todo_panel_uses_content_for_pending_item() {
        // Pending item: orig uses `content` ("DoThing"); mutant `==`->`!=`
        // (InProgress filter) uses `active_form` ("DoingThing") because
        // Pending != InProgress. "DoingThing" does not contain "DoThing"
        // as a substring, so checking for "DoThing" distinguishes them.
        // kills 233:41 `==`->`!=`.
        let mut app = App::new();
        app.current_todos = vec![todo("DoThing", TodoStatus::Pending, Some("DoingThing"))];
        app.turn.running = true;
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains("DoThing"),
            "Pending item should show content 'DoThing'"
        );
    }

    #[test]
    fn todo_window_centers_anchor() {
        // Goal-384 headline logic: the pure window helper must keep the
        // in-progress item visible when the list overflows the panel.
        // 9 items, 4 content rows, anchor at index 7 → window [5, 9),
        // which contains index 7 (the old hardcoded first-6 slice
        // showed [0, 6)).
        assert_eq!(todo_window(9, Some(7), 4), (5, 9));

        // Anchor at the very start → window pinned to the top.
        assert_eq!(todo_window(9, Some(0), 4), (0, 4));

        // Anchor at the last index → window pinned to the bottom.
        assert_eq!(todo_window(9, Some(8), 4), (5, 9));

        // List fits entirely → no scroll, full list shown.
        assert_eq!(todo_window(3, Some(2), 4), (0, 3));

        // No in-progress item → pin to the tail (most recent activity).
        assert_eq!(todo_window(9, None, 4), (5, 9));

        // Degenerate inputs → empty window, no panic.
        assert_eq!(todo_window(0, None, 4), (0, 0));
        assert_eq!(todo_window(9, Some(7), 0), (0, 0));
    }

    #[test]
    fn render_todo_panel_shows_in_progress_when_beyond_viewport() {
        // Goal-384 headline render test: with 9 todos on a 24-row screen
        // the panel shows only 6 content rows, so the old hardcoded
        // first-6 render would truncate the in-progress item at index 7
        // off-screen. The windowed render must centre on it and paint its
        // active_form label, plus show the truncation indicator.
        let mut app = App::new();
        app.turn.running = true;
        app.current_todos = (0..9)
            .map(|i| {
                if i == 7 {
                    todo("task7", TodoStatus::InProgress, Some("DoingTask7"))
                } else {
                    todo(&format!("task{i}"), TodoStatus::Pending, None)
                }
            })
            .collect();
        let buf = draw(&mut app, 80, 24);
        let text = all_text(&buf);
        assert!(
            text.contains("DoingTask7"),
            "in-progress item (index 7) must be visible; got:\n{text}"
        );
        // The list is longer than the panel (9 > 6 content rows) → the
        // truncation indicator reports 3 items scrolled off the top and
        // none off the bottom.
        assert!(
            text.contains("↑3 ↓0"),
            "expected truncation indicator '↑3 ↓0'; got:\n{text}"
        );
    }

    #[test]
    fn render_todo_panel_hides_indicator_when_everything_fits() {
        // When the whole list fits in the panel nothing is hidden, so the
        // title must NOT carry the truncation indicator. Kills the
        // `>` -> `>=` mutants on `hidden_top > 0 || hidden_bottom > 0`
        // (364:19 and 364:40): with `>=`, `hidden_bottom == 0` makes the
        // second condition always true and the title would read "↑0 ↓0".
        let mut app = App::new();
        app.turn.running = true;
        app.current_todos = (0..3)
            .map(|i| {
                if i == 1 {
                    todo("mid", TodoStatus::InProgress, Some("DoingMid"))
                } else {
                    todo(&format!("t{i}"), TodoStatus::Pending, None)
                }
            })
            .collect();
        let buf = draw(&mut app, 80, 24);
        let text = all_text(&buf);
        // Sanity: the in-progress item is visible (list fits → full list
        // shown, no truncation).
        assert!(
            text.contains("DoingMid"),
            "in-progress item must be visible; got:\n{text}"
        );
        // The title row must not show ↑N/↓M — the list fits entirely.
        let y = row_y_containing(&buf, " Tasks ").expect("todo panel title row");
        let mut title_row = String::new();
        for x in 0..buf.area.width {
            title_row.push_str(buf.cell((x, y)).expect("cell").symbol());
        }
        assert!(
            !title_row.contains('↑') && !title_row.contains('↓'),
            "no truncation indicator expected when everything fits; got: {title_row:?}"
        );
    }

    #[test]
    fn render_scrolls_up_without_panicking() {
        // kills 117:32 `-`->`+` in `max_scroll - capped`: with a scroll
        // offset larger than the visible height, the mutant computes
        // `start = max_scroll + capped`, which overshoots `total_rows` and
        // panics indexing `physical[start..end]`. orig clamps to a valid
        // window. We only assert the draw completes (no panic).
        let mut app = App::new();
        app.blocks = (0..20)
            .map(|i| TranscriptBlock::User {
                text: format!("row{i}a\nrow{i}b\nrow{i}c"),
            })
            .collect();
        app.scroll_offset = 10;
        let _ = draw(&mut app, 80, 12);
    }

    #[test]
    fn render_shows_modal_when_modals_nonempty() {
        // kills delete `!` in `if !app.modals.is_empty()` (155:8): the
        // mutant skips `modal::render` when modals are non-empty, so the
        // modal title " Help " never appears.
        let mut app = App::new();
        app.turn.running = true;
        app.modals = vec![Modal::Help];
        let buf = draw(&mut app, 80, 24);
        assert!(
            all_text(&buf).contains(" Help "),
            "expected the Help modal title to be rendered"
        );
    }

    // ── Goal-349: visible_physical_rows == the painted window ──────────

    #[test]
    fn visible_physical_rows_matches_painted_window() {
        // Pins the Goal-349 refactor: `visible_physical_rows` must return
        // exactly the `window` slice `render` paints (same length, same
        // rows in the same order), so selection/copy text can never diverge
        // from the screen. The messages panel is the top layout chunk, so
        // screen row y == window row y for y < window.len().
        use crate::harness::Harness;
        use crate::model::TranscriptBlock;

        let mut h = Harness::new();
        h.app_mut().blocks = (0..40)
            .map(|i| TranscriptBlock::System {
                text: format!("msg {i:02}"),
            })
            .collect();

        // Scroll up a bit, render, then verify the helper reproduces the
        // painted window row-for-row.
        h.app_mut().scroll_offset = 30;
        let screen = h.render();
        let rows = visible_physical_rows(h.app(), screen.width());
        assert!(!rows.is_empty(), "visible window must not be empty");
        for (i, row) in rows.iter().enumerate() {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(
                screen.line(i as u16),
                text.trim_end(),
                "visible_physical_rows row {i} must equal the painted row"
            );
        }

        // Scrolling to the bottom must shift the window the same way the
        // renderer paints it: same height, different visible content.
        // (Compare the full joined text — individual "last rows" can both be
        // blank separators depending on row parity.)
        h.app_mut().scroll_offset = 0;
        let screen2 = h.render();
        let rows2 = visible_physical_rows(h.app(), screen2.width());
        assert_eq!(
            rows.len(),
            rows2.len(),
            "window height is stable across scroll positions"
        );
        let text_of = |rs: &[Line<'static>]| -> String {
            rs.iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_ne!(
            text_of(&rows),
            text_of(&rows2),
            "scrolling must change which rows are visible"
        );
    }
}

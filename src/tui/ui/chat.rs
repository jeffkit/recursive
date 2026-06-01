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
//! While a turn is running the spinner from
//! [`crate::tui::ui::spinner::format_line`] is appended after the last
//! block.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::app::App;
use crate::tui::ui::{command_menu, input, modal, spinner, status, transcript};

pub fn render(frame: &mut Frame, app: &App) {
    let input_total = input::total_height(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),              // messages
            Constraint::Length(1),           // status bar
            Constraint::Length(input_total), // input + footer hint
        ])
        .split(frame.area());

    // Messages panel.
    let mut lines = transcript::render_blocks(&app.blocks, &app.usage);
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

    // Status bar.
    status::render(frame, chunks[1], app);

    // Input panel + footer hint.
    input::render(frame, chunks[2], app);

    // Goal-146: floating slash-command popup, drawn after the input
    // box so it overlays the messages panel.
    command_menu::render(frame, chunks[2], app);

    // Goal-146: modals are last so they cover everything else.
    if !app.modals.is_empty() {
        modal::render(frame, app);
    }
}

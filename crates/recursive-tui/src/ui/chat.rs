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
//! While a turn is running the spinner from
//! [`crate::ui::spinner::format_line`] is appended after the last
//! block.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;
use crate::ui::{input, spinner, status, transcript};

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

    let total_lines = lines.len() as u16;
    let visible_lines = chunks[0].height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(visible_lines);
    let effective_scroll = max_scroll.saturating_sub(app.scroll_offset.min(max_scroll));

    let messages = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));
    frame.render_widget(messages, chunks[0]);

    // Status bar.
    status::render(frame, chunks[1], app);

    // Input panel + footer hint.
    input::render(frame, chunks[2], app);
}

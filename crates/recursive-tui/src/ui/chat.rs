//! Chat screen renderer.
//!
//! Lays out the messages panel, status bar, and input panel. This is
//! a verbatim port of the pre-revamp `ui()` function — visuals must
//! stay byte-for-byte identical in goal-143.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // messages
            Constraint::Length(1), // status bar
            Constraint::Length(3), // input
        ])
        .split(frame.area());

    // Messages panel with styled lines and scroll support
    let lines: Vec<Line> = app.messages.iter().map(|m| m.to_line()).collect();
    let total_lines = lines.len() as u16;
    // The visible area is the chunk height minus 2 for borders
    let visible_lines = chunks[0].height.saturating_sub(2);
    // Clamp scroll_offset so we don't scroll past the content
    let max_scroll = total_lines.saturating_sub(visible_lines);
    // scroll_offset=0 means "at bottom"; convert to ratatui scroll (from top)
    let effective_scroll = max_scroll.saturating_sub(app.scroll_offset.min(max_scroll));
    let messages = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));
    frame.render_widget(messages, chunks[0]);

    // Status bar
    let status_text = if app.connected {
        let session_display = app
            .session_id
            .as_ref()
            .map(|id| &id[..id.len().min(8)])
            .unwrap_or("none");
        let msg_count = app.messages.len();
        format!(" Connected | Session: {session_display} | Messages: {msg_count}")
    } else {
        " Not connected".to_string()
    };
    let status_bar =
        Paragraph::new(status_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status_bar, chunks[1]);

    // Input panel with visual cursor
    let display_input = format!("{}▌", app.input);
    let input = Paragraph::new(display_input)
        .block(Block::default().borders(Borders::ALL).title(" Input "));
    frame.render_widget(input, chunks[2]);
}

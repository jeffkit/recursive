//! Plan review screen renderer.
//!
//! Shows the proposed plan inside a bordered panel above a status bar
//! that lists the y/n/e key bindings.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub fn render(frame: &mut Frame, plan_text: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // plan text
            Constraint::Length(1), // status bar
            Constraint::Length(3), // empty bottom
        ])
        .split(frame.area());

    // Plan text panel
    let lines: Vec<Line> = plan_text
        .lines()
        .map(|l| Line::from(l.to_string()).style(Style::default().fg(Color::White)))
        .collect();
    let plan = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Plan Proposal "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(plan, chunks[0]);

    // Status bar with keybindings
    let status_bar = Paragraph::new(" [Enter/y] Approve  [n/Esc] Reject  [e] Edit ")
        .style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(status_bar, chunks[1]);

    // Empty bottom panel
    let empty = Paragraph::new("").block(Block::default().borders(Borders::ALL));
    frame.render_widget(empty, chunks[2]);
}

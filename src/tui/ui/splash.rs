//! Splash screen renderer.
//!
//! Shows the centred Recursive logo. Auto-dismissed by `main` after a
//! short delay or any key press.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

pub fn render(frame: &mut Frame) {
    let area = frame.area();

    let logo_lines = vec![
        Line::from(""),
        Line::from("   ╱╲    Recursive Agent".to_string()).style(Style::default().fg(Color::Cyan)),
        Line::from("  ╱  ╲   ─────────────────".to_string())
            .style(Style::default().fg(Color::Cyan)),
        Line::from(format!(" ╱ ╱╲ ╲  v{}", env!("CARGO_PKG_VERSION"))).style(Style::default().fg(Color::White)),
        Line::from(" ╲ ╲╱ ╱".to_string()).style(Style::default().fg(Color::Cyan)),
        Line::from("  ╲  ╱   Self-improving AI agent".to_string())
            .style(Style::default().fg(Color::Cyan)),
        Line::from("   ╲╱    in Rust".to_string()).style(Style::default().fg(Color::Cyan)),
        Line::from(""),
        Line::from("  Press any key to continue...".to_string())
            .style(Style::default().fg(Color::DarkGray)),
    ];

    let logo_height = logo_lines.len() as u16;
    let vertical_pad = area.height.saturating_sub(logo_height) / 2;

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_pad),
            Constraint::Length(logo_height),
            Constraint::Min(0),
        ])
        .split(area);

    let paragraph = Paragraph::new(logo_lines).alignment(Alignment::Center);
    frame.render_widget(paragraph, layout[1]);
}

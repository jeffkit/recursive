//! Pure rendering: takes an [`App`] and draws it onto a ratatui frame.
//!
//! Kept separate from [`crate::app::App`] so the snapshot tests can
//! drive the rendering against a `TestBackend` without instantiating
//! a real terminal.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, MessageRole, Pane, ToolCallState};

/// Render the entire UI for the current frame.
pub fn render(f: &mut Frame, app: &App) {
    let size = f.area();
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(40), Constraint::Length(28)])
        .split(size);

    let left = outer[0];
    let right = outer[1];

    // Vertical: messages | (optional permission prompt) | input
    let prompt_height: u16 = if app.pending_permission.is_some() {
        5
    } else {
        0
    };
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(prompt_height),
            Constraint::Length(3),
        ])
        .split(left);

    render_messages(f, app, left_chunks[0]);
    if app.pending_permission.is_some() {
        render_permission(f, app, left_chunks[1]);
    }
    render_input(f, app, left_chunks[2]);
    render_state(f, app, right);
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn render_messages(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Messages")
        .border_style(focus_style(app.focus == Pane::Messages));

    let mut lines: Vec<Line> = Vec::new();
    for (idx, msg) in app.messages.iter().enumerate() {
        let prefix = match msg.role {
            MessageRole::User => format!("#{} user:", idx + 1),
            MessageRole::Assistant => format!("#{} assistant:", idx + 1),
            MessageRole::System => format!("#{} system:", idx + 1),
        };
        let style = match msg.role {
            MessageRole::User => Style::default().fg(Color::Green),
            MessageRole::Assistant => Style::default().fg(Color::White),
            MessageRole::System => Style::default().fg(Color::DarkGray),
        };
        // Body: prefix on same line as first content line.
        let first_body = msg.text.lines().next().unwrap_or("");
        lines.push(Line::from(vec![
            Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw(first_body.to_string()),
        ]));
        for extra in msg.text.lines().skip(1) {
            lines.push(Line::from(extra.to_string()));
        }

        // Tool calls inlined under their parent message.
        for tc_id in &msg.tool_calls {
            if let Some(tc) = app.tool_calls.get(tc_id) {
                lines.push(Line::from(vec![
                    Span::styled("  ⚙ ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        tc.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("("),
                    Span::raw(truncate(&tc.args, 60)),
                    Span::raw(")"),
                ]));
                if let Some(result) = &tc.result {
                    lines.push(Line::from(vec![
                        Span::styled("  ↪ ", Style::default().fg(Color::Yellow)),
                        Span::raw(truncate(result, 80)),
                    ]));
                } else if matches!(tc.state, ToolCallState::AwaitingResult) {
                    lines.push(Line::from(vec![
                        Span::styled("  ↪ ", Style::default().fg(Color::Yellow)),
                        Span::styled("(running…)", Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
        }
    }

    // Orphan tool calls (no parent message recorded) — render them at
    // the end so the user still sees them.
    let orphans: Vec<&String> = app
        .tool_call_order
        .iter()
        .filter(|id| {
            app.tool_calls
                .get(*id)
                .map(|tc| tc.parent_message_id.is_none())
                .unwrap_or(false)
                && !app.messages.iter().any(|m| m.tool_calls.contains(id))
        })
        .collect();
    if !orphans.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "(tool calls without parent message)",
            Style::default().fg(Color::DarkGray),
        )));
        for id in orphans {
            if let Some(tc) = app.tool_calls.get(id) {
                lines.push(Line::from(format!(
                    "  ⚙ {}({})",
                    tc.name,
                    truncate(&tc.args, 60)
                )));
                if let Some(r) = &tc.result {
                    lines.push(Line::from(format!("  ↪ {}", truncate(r, 80))));
                }
            }
        }
    }

    let scroll = if app.focus == Pane::Messages {
        app.messages_scroll
    } else {
        0
    };
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn render_permission(f: &mut Frame, app: &App, area: Rect) {
    let prompt = app.pending_permission.as_ref().expect("prompt set");
    let lines = vec![
        Line::from(Span::styled(
            "⚠ permission requested:",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "  {} {}",
            prompt.tool,
            truncate(&prompt.args_preview, 60)
        )),
        Line::from("  [y] approve  [n] reject  [esc] later"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Permission")
        .border_style(Style::default().fg(Color::Yellow));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Input")
        .border_style(focus_style(app.focus == Pane::Input));
    let cursor = if app.focus == Pane::Input { "_" } else { "" };
    let line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(app.input_buffer.clone()),
        Span::styled(
            cursor.to_string(),
            Style::default().add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    let para = Paragraph::new(line).block(block);
    f.render_widget(para, area);
}

fn render_state(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("State");
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!(
        "thread: {}",
        truncate(&app.state.thread_id, 18)
    )));
    lines.push(Line::from(format!(
        "run:    {}",
        app.state
            .run_id
            .as_deref()
            .map(|s| truncate(s, 18))
            .unwrap_or_else(|| "—".into())
    )));
    lines.push(Line::from(format!("steps:  {}", app.state.steps)));
    lines.push(Line::from(format!("tokens: {}", app.state.tokens)));
    if let Some(ms) = app.state.last_heartbeat_ms {
        let secs = (ms as f64) / 1000.0;
        lines.push(Line::from(format!("⏱ {:.1}s", secs)));
    } else if app.state.running {
        lines.push(Line::from("running…"));
    }
    if let Some((turn, post)) = &app.state.last_checkpoint {
        lines.push(Line::from(format!(
            "ckpt:   {} (t{})",
            truncate(post, 12),
            turn
        )));
    }
    if !app.state.tool_counts.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tools used",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let mut counts: Vec<(&String, &u32)> = app.state.tool_counts.iter().collect();
        counts.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (name, count) in counts {
            lines.push(Line::from(format!("• {} ×{}", name, count)));
        }
    }
    if let Some(s) = &app.status {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            truncate(s, 24),
            Style::default().fg(Color::Red),
        )));
    }
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out.replace('\n', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use agui_protocol::{
        BaseEvent, Custom, Event, TextMessageContent, TextMessageEnd, TextMessageStart,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn buffer_to_string(t: &Terminal<TestBackend>) -> String {
        let buf = t.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_initial_layout() {
        let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let app = App::new("thr".into());
        t.draw(|f| render(f, &app)).unwrap();
        let s = buffer_to_string(&t);
        // Border titles must be present.
        assert!(s.contains("Messages"), "missing Messages border:\n{s}");
        assert!(s.contains("Input"), "missing Input border:\n{s}");
        assert!(s.contains("State"), "missing State border:\n{s}");
        // Sidebar reflects the (empty) thread we initialised with.
        assert!(s.contains("thread: thr"), "missing thread row:\n{s}");
        assert!(s.contains("steps:  0"), "missing steps row:\n{s}");
    }

    #[test]
    fn renders_streamed_assistant_message() {
        let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let mut app = App::new("thr".into());
        app.apply_event(Event::TextMessageStart(TextMessageStart {
            message_id: "m-1".into(),
            role: Some("assistant".into()),
            base: BaseEvent::default(),
        }));
        for delta in ["Hel", "lo, ", "world!"] {
            app.apply_event(Event::TextMessageContent(TextMessageContent {
                message_id: "m-1".into(),
                delta: delta.into(),
                base: BaseEvent::default(),
            }));
        }
        app.apply_event(Event::TextMessageEnd(TextMessageEnd {
            message_id: "m-1".into(),
            base: BaseEvent::default(),
        }));
        t.draw(|f| render(f, &app)).unwrap();
        let s = buffer_to_string(&t);
        assert!(
            s.contains("Hello, world!"),
            "expected concatenated text in messages pane:\n{s}"
        );
        assert!(s.contains("assistant"), "expected role label:\n{s}");
    }

    #[test]
    fn permission_prompt_appears_on_custom_event() {
        let mut t = Terminal::new(TestBackend::new(80, 18)).unwrap();
        let mut app = App::new("thr".into());
        app.apply_event(Event::Custom(Custom {
            name: "agui-tui/permission_request".into(),
            value: json!({
                "interruptId": "i-1",
                "tool": "run_shell",
                "argsPreview": "cargo test",
            }),
            base: BaseEvent::default(),
        }));
        t.draw(|f| render(f, &app)).unwrap();
        let s = buffer_to_string(&t);
        assert!(s.contains("permission requested"), "missing prompt:\n{s}");
        assert!(s.contains("run_shell"), "missing tool name:\n{s}");
        assert!(s.contains("[y] approve"), "missing keybinding hint:\n{s}");
    }
}

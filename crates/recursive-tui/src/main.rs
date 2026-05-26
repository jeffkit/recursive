use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq)]
enum StyledMessage {
    User(String),
    Assistant(String),
    ToolCall(String),
    ToolResult { name: String, success: bool },
    System(String),
}

impl StyledMessage {
    fn to_line(&self) -> Line<'_> {
        match self {
            Self::User(text) => {
                Line::from(format!("You: {text}")).style(Style::default().fg(Color::White))
            }
            Self::Assistant(text) => {
                Line::from(format!("Agent: {text}")).style(Style::default().fg(Color::Cyan))
            }
            Self::ToolCall(name) => Line::from(format!("  🔧 {name}"))
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)),
            Self::ToolResult { name, success } => {
                let prefix = if *success { "  ✓" } else { "  ✗" };
                let color = if *success { Color::Green } else { Color::Red };
                Line::from(format!("{prefix} {name}"))
                    .style(Style::default().fg(color).add_modifier(Modifier::DIM))
            }
            Self::System(text) => Line::from(text.clone())
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
        }
    }
}

#[derive(Debug)]
enum UiEvent {
    ToolCall { name: String },
    ToolResult { name: String, success: bool },
    AssistantMessage { content: String },
    Error { message: String },
}

struct App {
    input: String,
    messages: Vec<StyledMessage>,
    should_quit: bool,
    session_id: Option<String>,
    base_url: String,
    connected: bool,
}

impl App {
    fn new() -> Self {
        Self {
            input: String::new(),
            messages: vec![StyledMessage::System(
                "Welcome to Recursive TUI. Type a message and press Enter.".into(),
            )],
            should_quit: false,
            session_id: None,
            base_url: "http://127.0.0.1:3000".into(),
            connected: false,
        }
    }

    async fn try_connect(&mut self) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap_or_default();

        // Health check
        let health_url = format!("{}/health", self.base_url);
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                self.connected = true;
                self.messages.push(StyledMessage::System("Connected to server.".into()));
            }
            _ => {
                self.connected = false;
                self.messages.push(StyledMessage::System(
                    "Not connected — running offline.".into(),
                ));
                return;
            }
        }

        // Create session
        let session_url = format!("{}/sessions", self.base_url);
        match client.post(&session_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
                        self.session_id = Some(id.to_string());
                    }
                }
            }
            _ => {
                self.messages.push(StyledMessage::System(
                    "Failed to create session.".into(),
                ));
            }
        }
    }

    async fn handle_key(&mut self, key: KeyCode, event_tx: &mpsc::UnboundedSender<UiEvent>) {
        match key {
            KeyCode::Enter => {
                if !self.input.is_empty() && self.session_id.is_some() {
                    let msg = self.input.clone();
                    self.messages.push(StyledMessage::User(msg.clone()));
                    self.input.clear();

                    let session_id = self.session_id.clone().unwrap();
                    let base_url = self.base_url.clone();
                    let tx = event_tx.clone();

                    tokio::spawn(async move {
                        let client = reqwest::Client::builder()
                            .no_proxy()
                            .build()
                            .unwrap_or_default();

                        let url = format!("{base_url}/sessions/{session_id}/messages");
                        match client
                            .post(&url)
                            .json(&serde_json::json!({"content": msg}))
                            .send()
                            .await
                        {
                            Ok(resp) => {
                                if let Ok(body) = resp.json::<serde_json::Value>().await {
                                    // Check for tool calls in the response
                                    if let Some(tools) = body.get("tool_calls").and_then(|v| v.as_array()) {
                                        for tool in tools {
                                            if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                                                let _ = tx.send(UiEvent::ToolCall {
                                                    name: name.to_string(),
                                                });
                                            }
                                        }
                                    }

                                    if let Some(results) = body.get("tool_results").and_then(|v| v.as_array()) {
                                        for result in results {
                                            let name = result
                                                .get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let success = result
                                                .get("success")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(true);
                                            let _ = tx.send(UiEvent::ToolResult { name, success });
                                        }
                                    }

                                    let content = body
                                        .get("content")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    if !content.is_empty() {
                                        let _ = tx.send(UiEvent::AssistantMessage { content });
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(UiEvent::Error {
                                    message: e.to_string(),
                                });
                            }
                        }
                    });
                } else if !self.input.is_empty() && self.session_id.is_none() {
                    // Offline mode: echo the message but note no connection
                    let msg = self.input.clone();
                    self.messages.push(StyledMessage::User(msg));
                    self.messages.push(StyledMessage::System(
                        "No active session — message not sent.".into(),
                    ));
                    self.input.clear();
                }
            }
            KeyCode::Char('q') if self.input.is_empty() => {
                self.should_quit = true;
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::ToolCall { name } => {
                self.messages.push(StyledMessage::ToolCall(name));
            }
            UiEvent::ToolResult { name, success } => {
                self.messages.push(StyledMessage::ToolResult { name, success });
            }
            UiEvent::AssistantMessage { content } => {
                self.messages.push(StyledMessage::Assistant(content));
            }
            UiEvent::Error { message } => {
                self.messages
                    .push(StyledMessage::System(format!("Error: {message}")));
            }
        }
    }
}

fn ui(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // messages
            Constraint::Length(3), // input
        ])
        .split(frame.area());

    // Messages panel with styled lines
    let lines: Vec<Line> = app.messages.iter().map(|m| m.to_line()).collect();
    let messages = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    // Input panel
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" Input "));
    frame.render_widget(input, chunks[1]);
}

#[tokio::main]
async fn main() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<UiEvent>();
    let mut app = App::new();

    // Try to connect
    app.try_connect().await;

    loop {
        terminal.draw(|frame| ui(frame, &app))?;

        tokio::select! {
            // Check for keyboard events (with timeout to not block)
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(50)) => {
                while event::poll(std::time::Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            app.handle_key(key.code, &event_tx).await;
                        }
                    }
                }
            }
            // Check for incoming UI events
            Some(ui_event) = event_rx.recv() => {
                app.handle_ui_event(ui_event);
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input.is_empty());
        assert!(!app.messages.is_empty()); // welcome message
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn enter_moves_input_to_messages() {
        let (tx, _rx) = mpsc::unbounded_channel::<UiEvent>();
        let mut app = App::new();
        // Set session_id to None so offline path is taken
        app.input = "hello".to_string();
        app.handle_key(KeyCode::Enter, &tx).await;
        assert!(app.input.is_empty());
        assert!(app.messages.iter().any(|m| matches!(m, StyledMessage::User(t) if t.contains("hello"))));
    }

    #[tokio::test]
    async fn esc_sets_should_quit() {
        let (tx, _rx) = mpsc::unbounded_channel::<UiEvent>();
        let mut app = App::new();
        app.handle_key(KeyCode::Esc, &tx).await;
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn char_appends_to_input() {
        let (tx, _rx) = mpsc::unbounded_channel::<UiEvent>();
        let mut app = App::new();
        app.handle_key(KeyCode::Char('h'), &tx).await;
        app.handle_key(KeyCode::Char('i'), &tx).await;
        assert_eq!(app.input, "hi");
    }

    #[tokio::test]
    async fn backspace_removes_last_char() {
        let (tx, _rx) = mpsc::unbounded_channel::<UiEvent>();
        let mut app = App::new();
        app.input = "hello".to_string();
        app.handle_key(KeyCode::Backspace, &tx).await;
        assert_eq!(app.input, "hell");
    }

    #[test]
    fn styled_message_user_to_line_contains_text() {
        let msg = StyledMessage::User("hello world".into());
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("You: hello world"));
    }

    #[test]
    fn styled_message_assistant_to_line_contains_text() {
        let msg = StyledMessage::Assistant("I can help".into());
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("Agent: I can help"));
    }

    #[test]
    fn styled_message_tool_call_to_line_contains_name() {
        let msg = StyledMessage::ToolCall("read_file".into());
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("🔧 read_file"));
    }

    #[test]
    fn styled_message_tool_result_success_to_line() {
        let msg = StyledMessage::ToolResult {
            name: "read_file".into(),
            success: true,
        };
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("✓ read_file"));
    }

    #[test]
    fn styled_message_tool_result_failure_to_line() {
        let msg = StyledMessage::ToolResult {
            name: "write_file".into(),
            success: false,
        };
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("✗ write_file"));
    }

    #[test]
    fn styled_message_system_to_line_contains_text() {
        let msg = StyledMessage::System("Connected".into());
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("Connected"));
    }

    #[test]
    fn handle_ui_event_adds_tool_call() {
        let mut app = App::new();
        app.handle_ui_event(UiEvent::ToolCall {
            name: "search".into(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::ToolCall(n)) if n == "search"
        ));
    }

    #[test]
    fn handle_ui_event_adds_tool_result() {
        let mut app = App::new();
        app.handle_ui_event(UiEvent::ToolResult {
            name: "search".into(),
            success: true,
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::ToolResult { name, success }) if name == "search" && *success
        ));
    }

    #[test]
    fn handle_ui_event_adds_assistant_message() {
        let mut app = App::new();
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Hello!".into(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::Assistant(t)) if t == "Hello!"
        ));
    }

    #[test]
    fn handle_ui_event_adds_error_as_system() {
        let mut app = App::new();
        app.handle_ui_event(UiEvent::Error {
            message: "timeout".into(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::System(t)) if t.contains("timeout")
        ));
    }

    #[test]
    fn app_no_session_shows_system_message() {
        let app = App::new();
        assert!(app.session_id.is_none());
        // The welcome message is a system message
        assert!(matches!(&app.messages[0], StyledMessage::System(t) if t.contains("Welcome")));
    }
}

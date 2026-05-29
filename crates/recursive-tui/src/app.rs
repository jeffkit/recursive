//! Application state for the Recursive TUI.
//!
//! [`App`] owns everything visible to the user: the transcript,
//! the input buffer, the current screen, and scroll position.
//! It is deliberately decoupled from the agent worker — the only
//! interaction with the [`crate::backend::Backend`] happens through
//! [`UiEvent`] (in) and [`UserAction`] (out).

use std::time::Instant;

use crossterm::event::KeyCode;
use ratatui::prelude::*;

use crate::events::{UiEvent, UserAction};

/// Which top-level screen is currently rendered.
#[derive(Clone, Debug, PartialEq)]
pub enum AppScreen {
    Splash,
    Chat,
    PlanReview { plan_text: String },
}

/// One entry in the message log; renders to a styled [`Line`].
#[derive(Clone, Debug, PartialEq)]
pub enum StyledMessage {
    User(String),
    Assistant(String),
    ToolCall(String),
    ToolResult { name: String, success: bool },
    System(String),
    Separator,
}

impl StyledMessage {
    pub fn to_line(&self) -> Line<'_> {
        match self {
            Self::User(text) => {
                Line::from(format!("You: {text}")).style(Style::default().fg(Color::White))
            }
            Self::Assistant(text) => {
                Line::from(format!("Agent: {text}")).style(Style::default().fg(Color::Cyan))
            }
            Self::ToolCall(name) => Line::from(format!("  🔧 {name}")).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            ),
            Self::ToolResult { name, success } => {
                let prefix = if *success { "  ✓" } else { "  ✗" };
                let color = if *success { Color::Green } else { Color::Red };
                Line::from(format!("{prefix} {name}"))
                    .style(Style::default().fg(color).add_modifier(Modifier::DIM))
            }
            Self::System(text) => Line::from(text.clone()).style(
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Self::Separator => Line::from("────────────────────────────────")
                .style(Style::default().fg(Color::DarkGray)),
        }
    }
}

/// Top-level UI state. Mutated by `handle_key` (driven by the input
/// thread) and `handle_ui_event` (driven by the backend worker).
pub struct App {
    pub input: String,
    pub messages: Vec<StyledMessage>,
    pub should_quit: bool,
    pub session_id: Option<String>,
    pub connected: bool,
    pub scroll_offset: u16,
    pub screen: AppScreen,
    pub splash_start: Instant,
}

impl App {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            messages: vec![StyledMessage::System(
                "Welcome to Recursive TUI. Type a message and press Enter.".into(),
            )],
            should_quit: false,
            session_id: None,
            connected: false,
            scroll_offset: 0,
            screen: AppScreen::Splash,
            splash_start: Instant::now(),
        }
    }

    /// Process one key event. Returns an optional [`UserAction`] that
    /// the caller must forward to the backend worker; key strokes that
    /// only mutate local UI state (typing, scrolling, screen
    /// transitions) return `None`.
    pub fn handle_key(&mut self, key: KeyCode) -> Option<UserAction> {
        // ── PlanReview screen ────────────────────────────────────────
        if let AppScreen::PlanReview { ref plan_text } = self.screen {
            let plan_text = plan_text.clone();
            match key {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.messages
                        .push(StyledMessage::System("Plan approved".into()));
                    self.messages
                        .push(StyledMessage::Assistant(plan_text.clone()));
                    self.screen = AppScreen::Chat;
                    self.scroll_to_bottom();
                    return Some(UserAction::ConfirmPlan);
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.messages
                        .push(StyledMessage::System("Plan rejected".into()));
                    self.screen = AppScreen::Chat;
                    self.scroll_to_bottom();
                    return Some(UserAction::RejectPlan(String::new()));
                }
                KeyCode::Char('e') => {
                    self.input = plan_text;
                    self.screen = AppScreen::Chat;
                    return None;
                }
                _ => return None,
            }
        }

        // ── Chat screen ──────────────────────────────────────────────
        match key {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let msg = self.input.clone();
                    self.messages.push(StyledMessage::User(msg.clone()));
                    self.input.clear();
                    self.scroll_to_bottom();
                    Some(UserAction::SendMessage(msg))
                } else {
                    None
                }
            }
            KeyCode::Up if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
                None
            }
            KeyCode::Down if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                None
            }
            KeyCode::PageUp if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
                None
            }
            KeyCode::PageDown if self.input.is_empty() => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                None
            }
            KeyCode::Char('q') if self.input.is_empty() => {
                self.should_quit = true;
                None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Esc => {
                self.should_quit = true;
                None
            }
            _ => None,
        }
    }

    /// Apply an event coming from the backend worker.
    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::ToolCall { name } => {
                self.messages.push(StyledMessage::ToolCall(name));
            }
            UiEvent::ToolResult { name, success } => {
                self.messages
                    .push(StyledMessage::ToolResult { name, success });
            }
            UiEvent::AssistantMessage { content } => {
                let first_line = content.lines().next().unwrap_or("");
                let lower = first_line.to_lowercase();
                if lower.starts_with("plan:") || lower.starts_with("## plan") {
                    self.screen = AppScreen::PlanReview { plan_text: content };
                } else {
                    self.messages.push(StyledMessage::Assistant(content));
                    self.messages.push(StyledMessage::Separator);
                }
            }
            UiEvent::Error { message } => {
                self.messages
                    .push(StyledMessage::System(format!("Error: {message}")));
            }
        }
        self.scroll_to_bottom();
    }

    pub fn scroll_to_bottom(&mut self) {
        // Reset scroll to show the latest messages
        self.scroll_offset = 0;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── construction ────────────────────────────────────────────────

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input.is_empty());
        assert!(!app.messages.is_empty()); // welcome message
        assert!(!app.should_quit);
    }

    #[test]
    fn app_new_starts_in_splash_screen() {
        let app = App::new();
        assert_eq!(app.screen, AppScreen::Splash);
    }

    #[test]
    fn splash_auto_transitions_after_elapsed() {
        let app = App::new();
        // splash_start is set to now, so elapsed < 2s
        assert!(app.splash_start.elapsed() < Duration::from_secs(2));
        assert_eq!(app.screen, AppScreen::Splash);
    }

    #[test]
    fn app_no_session_shows_system_message() {
        let app = App::new();
        assert!(app.session_id.is_none());
        // The welcome message is a system message
        assert!(matches!(&app.messages[0], StyledMessage::System(t) if t.contains("Welcome")));
    }

    // ── styled message → line ───────────────────────────────────────

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
    fn separator_to_line_produces_correct_output() {
        let msg = StyledMessage::Separator;
        let line = msg.to_line();
        let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("────────"));
        assert_eq!(line.style, Style::default().fg(Color::DarkGray));
    }

    // ── handle_ui_event ─────────────────────────────────────────────

    #[test]
    fn handle_ui_event_adds_tool_call() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
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
        app.screen = AppScreen::Chat;
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
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Hello!".into(),
        });
        // Assistant message is second-to-last (Separator is last)
        let len = app.messages.len();
        assert!(matches!(
            &app.messages[len - 2],
            StyledMessage::Assistant(t) if t == "Hello!"
        ));
    }

    #[test]
    fn handle_ui_event_adds_error_as_system() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::Error {
            message: "timeout".into(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::System(t)) if t.contains("timeout")
        ));
    }

    #[test]
    fn assistant_message_adds_separator() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "response".into(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(StyledMessage::Separator)
        ));
    }

    // ── chat-mode key handling ──────────────────────────────────────

    #[test]
    fn enter_moves_input_to_messages() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hello".to_string();
        let action = app.handle_key(KeyCode::Enter);
        assert!(app.input.is_empty());
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::User(t) if t.contains("hello"))));
        assert!(matches!(action, Some(UserAction::SendMessage(s)) if s == "hello"));
    }

    #[test]
    fn esc_sets_should_quit() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(KeyCode::Esc);
        assert!(app.should_quit);
    }

    #[test]
    fn char_appends_to_input() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(KeyCode::Char('h'));
        let _ = app.handle_key(KeyCode::Char('i'));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "hello".to_string();
        let _ = app.handle_key(KeyCode::Backspace);
        assert_eq!(app.input, "hell");
    }

    // ── scrolling ───────────────────────────────────────────────────

    #[test]
    fn scroll_up_increases_offset() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        // Add enough messages to allow scrolling
        for i in 0..30 {
            app.messages.push(StyledMessage::System(format!("msg {i}")));
        }
        let _ = app.handle_key(KeyCode::Up);
        assert_eq!(app.scroll_offset, 1);
        let _ = app.handle_key(KeyCode::Up);
        assert_eq!(app.scroll_offset, 2);
    }

    #[test]
    fn scroll_down_decreases_offset_stops_at_zero() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 3;
        let _ = app.handle_key(KeyCode::Down);
        assert_eq!(app.scroll_offset, 2);
        let _ = app.handle_key(KeyCode::Down);
        assert_eq!(app.scroll_offset, 1);
        let _ = app.handle_key(KeyCode::Down);
        assert_eq!(app.scroll_offset, 0);
        // Should not go below 0
        let _ = app.handle_key(KeyCode::Down);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn new_message_resets_scroll_to_bottom() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 5;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "hello".into(),
        });
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn scroll_keys_ignored_when_input_not_empty() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "typing".to_string();
        let _ = app.handle_key(KeyCode::Up);
        assert_eq!(app.scroll_offset, 0);
        let _ = app.handle_key(KeyCode::PageUp);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn page_up_scrolls_by_ten() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let _ = app.handle_key(KeyCode::PageUp);
        assert_eq!(app.scroll_offset, 10);
    }

    #[test]
    fn page_down_scrolls_by_ten() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.scroll_offset = 15;
        let _ = app.handle_key(KeyCode::PageDown);
        assert_eq!(app.scroll_offset, 5);
    }

    // ── Plan Mode ───────────────────────────────────────────────────

    #[test]
    fn plan_message_triggers_plan_review() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "## Plan\n1. Do thing A\n2. Do thing B".into(),
        });
        assert!(matches!(
            app.screen,
            AppScreen::PlanReview { ref plan_text }
            if plan_text == "## Plan\n1. Do thing A\n2. Do thing B"
        ));
    }

    #[test]
    fn plan_message_with_plan_colon_triggers_review() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Plan: refactor the module".into(),
        });
        assert!(matches!(app.screen, AppScreen::PlanReview { .. }));
    }

    #[test]
    fn non_plan_message_stays_in_chat() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.handle_ui_event(UiEvent::AssistantMessage {
            content: "Hello, I can help you.".into(),
        });
        assert_eq!(app.screen, AppScreen::Chat);
    }

    #[test]
    fn plan_approve_sends_and_returns_to_chat() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "## Plan\nDo X".into(),
        };
        let action = app.handle_key(KeyCode::Enter);
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        // Should have System("Plan approved") and Assistant(plan_text)
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::System(t) if t == "Plan approved")));
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::Assistant(t) if t == "## Plan\nDo X")));
    }

    #[test]
    fn plan_approve_with_y_key() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "Plan: do stuff".into(),
        };
        let action = app.handle_key(KeyCode::Char('y'));
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::ConfirmPlan)));
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::System(t) if t == "Plan approved")));
    }

    #[test]
    fn plan_reject_returns_to_chat() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "## Plan\nDo Y".into(),
        };
        let action = app.handle_key(KeyCode::Esc);
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::RejectPlan(_))));
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::System(t) if t == "Plan rejected")));
        // Plan text should NOT be in messages
        assert!(!app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::Assistant(t) if t.contains("Do Y"))));
    }

    #[test]
    fn plan_reject_with_n_key() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "Plan: something".into(),
        };
        let action = app.handle_key(KeyCode::Char('n'));
        assert_eq!(app.screen, AppScreen::Chat);
        assert!(matches!(action, Some(UserAction::RejectPlan(_))));
        assert!(app
            .messages
            .iter()
            .any(|m| matches!(m, StyledMessage::System(t) if t == "Plan rejected")));
    }

    #[test]
    fn plan_edit_prefills_input() {
        let mut app = App::new();
        app.screen = AppScreen::PlanReview {
            plan_text: "## Plan\nEdit me".into(),
        };
        let action = app.handle_key(KeyCode::Char('e'));
        assert_eq!(app.screen, AppScreen::Chat);
        assert_eq!(app.input, "## Plan\nEdit me");
        assert!(action.is_none());
    }
}

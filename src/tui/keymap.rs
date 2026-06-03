//! Key → [`UserAction`] mapping.
//!
//! Goal-144 widens this from a thin `KeyCode` forwarder to a full
//! `KeyEvent` forwarder so that modifier-aware bindings (the new
//! `Ctrl+E` toggle on the most recent `ToolResult` block) are handled
//! uniformly inside `App::handle_key`.
//!
//! Goal-145 will introduce mode-aware mapping (Insert / Command /
//! Visual …) and that logic will grow here.

use crossterm::event::KeyEvent;

use crate::tui::app::App;
use crate::tui::events::UserAction;

/// Dispatch a key event onto the app state. Returns an optional
/// [`UserAction`] the caller must forward to the agent worker.
pub fn dispatch(app: &mut App, key: KeyEvent) -> Option<UserAction> {
    app.handle_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{AppScreen, ToolResultData, TranscriptBlock};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn dispatch_routes_chat_enter_to_send_message() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.set_input("ping");
        let action = dispatch(&mut app, k(KeyCode::Enter));
        assert!(matches!(action, Some(UserAction::SendMessage(s)) if s == "ping"));
    }

    #[test]
    fn dispatch_routes_chat_typing_to_no_action() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let action = dispatch(&mut app, k(KeyCode::Char('a')));
        assert!(action.is_none());
        assert_eq!(app.input(), "a");
    }

    #[test]
    fn dispatch_ctrl_e_toggles_last_tool_result() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.blocks.push(TranscriptBlock::ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            args_preview: String::new(),
            result: Some(ToolResultData {
                success: true,
                output: "abc".into(),
                expanded: false,
            }),
        });
        let _ = dispatch(&mut app, ctrl('e'));
        match app.blocks.last() {
            Some(TranscriptBlock::ToolCall {
                result: Some(ToolResultData { expanded, .. }),
                ..
            }) => assert!(*expanded),
            other => panic!("expected ToolCall with Some(result), got {other:?}"),
        }
    }
}

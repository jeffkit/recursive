//! Key → [`UserAction`] mapping.
//!
//! This module is a deliberately thin wrapper today: it forwards every
//! key into [`App::handle_key`], which already encodes the existing
//! goal-143 key bindings. Goal-145 will introduce mode-aware mapping
//! (Insert / Command / Visual …) and this is where that logic will
//! grow.
//!
//! Keeping the indirection now means callers (the main loop, integration
//! tests) only depend on the keymap surface, not on `App` internals.

use crossterm::event::KeyCode;

use crate::app::App;
use crate::events::UserAction;

/// Dispatch a key event onto the app state. Returns an optional
/// [`UserAction`] the caller must forward to the agent worker.
pub fn dispatch(app: &mut App, key: KeyCode) -> Option<UserAction> {
    app.handle_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppScreen;

    #[test]
    fn dispatch_routes_chat_enter_to_send_message() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.input = "ping".into();
        let action = dispatch(&mut app, KeyCode::Enter);
        assert!(matches!(action, Some(UserAction::SendMessage(s)) if s == "ping"));
    }

    #[test]
    fn dispatch_routes_chat_typing_to_no_action() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        let action = dispatch(&mut app, KeyCode::Char('a'));
        assert!(action.is_none());
        assert_eq!(app.input, "a");
    }
}

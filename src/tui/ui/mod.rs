//! Top-level rendering dispatch.
//!
//! The single entry point [`render`] delegates to the chat renderer.
//! The TUI runs full-screen on the alternate screen; when the transcript
//! is empty the chat renderer draws a centred startup splash (logo + hints)
//! instead of a separate splash screen — see
//! [`crate::tui::ui::chat::render`].

use ratatui::Frame;

use crate::tui::app::App;

pub mod chat;
pub mod command_menu;
pub mod diff;
pub mod input;
pub mod markdown;
pub mod modal;
pub mod spinner;
pub mod status;
pub mod theme;
pub mod transcript;

pub use theme::{find_theme, Theme, DARK};

/// Render the current screen onto `frame`.
pub fn render(frame: &mut Frame, app: &App) {
    chat::render(frame, app);
}

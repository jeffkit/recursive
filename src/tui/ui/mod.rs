//! Top-level rendering dispatch.
//!
//! The single entry point [`render`] delegates to the chat renderer.
//! The splash screen was removed in favour of a startup banner printed
//! to stdout before the inline TUI viewport starts — see
//! [`crate::tui::print_startup_banner`].

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

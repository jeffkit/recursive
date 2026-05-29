//! Top-level rendering dispatch.
//!
//! The single entry point [`render`] picks a screen renderer based on
//! the current [`AppScreen`]. Each renderer lives in its own
//! sub-module so future iterations can extend layouts without bloating
//! the chat view.
//!
//! Goal 147 collapsed the dedicated `PlanReview` screen into the
//! modal stack — see [`modal::render_plan_review`].

use ratatui::Frame;

use crate::app::{App, AppScreen};

pub mod chat;
pub mod command_menu;
pub mod diff;
pub mod input;
pub mod modal;
pub mod spinner;
pub mod splash;
pub mod status;
pub mod transcript;

/// Render the current screen onto `frame`.
pub fn render(frame: &mut Frame, app: &App) {
    match &app.screen {
        AppScreen::Splash => splash::render(frame),
        AppScreen::Chat => chat::render(frame, app),
    }
}

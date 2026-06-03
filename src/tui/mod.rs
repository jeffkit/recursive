pub mod app;
pub mod backend;
pub mod bash;
pub mod commands;
pub mod completion;
pub mod cost;
pub mod events;
pub mod input_state;
pub mod keymap;
pub mod model;
pub mod runtime_builder;
pub mod skill_commands;
pub mod ui;

// Re-export types used outside the tui module.
pub use cost::UsageStats;
pub use input_state::{InputMode, PromptInputState};
pub use model::{AppScreen, DiffHunk, DiffLine, DiffLineKind, TranscriptBlock};

use std::io;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, MouseEvent, MouseEventKind};
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;

use crate::tui::app::App;
use crate::tui::backend::Backend;
use crate::tui::events::UserAction;

/// RAII guard that restores the terminal to its original state on drop.
///
/// Ensures cleanup happens even if the TUI panics or returns early with an
/// error, preventing the terminal from being left in raw/alternate-screen mode.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Launch the TUI and run until the user quits.
pub async fn run() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    // RAII guard: restores terminal on any exit path (normal, error, or panic).
    let _guard = TerminalGuard;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = Backend::spawn();
    let mut app = App::new();
    // Goal-161: share the permission-enabled flag so the UI (/permissions command)
    // can toggle the backend hook without a separate channel.
    app.permission_hook_enabled = backend.permission_enabled.clone();

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;
        app.spinner_frame = app.spinner_frame.wrapping_add(1);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            if app.screen == AppScreen::Splash {
                                app.screen = AppScreen::Chat;
                            } else if let Some(action) = keymap::dispatch(&mut app, key) {
                                let _ = backend.action_tx.send(action);
                            }
                        }
                        Event::Mouse(mev) => handle_mouse(&mut app, mev),
                        _ => {}
                    }
                }
            }
            Some(ui_event) = backend.event_rx.recv() => {
                app.handle_ui_event(ui_event);
            }
            // Goal-161: permission side-channel — delivers tool-call
            // permission requests from the backend worker to the UI.
            Some(perm_req) = backend.perm_rx.recv() => {
                app.set_pending_permission(perm_req);
            }
        }

        if app.screen == AppScreen::Splash && app.splash_start.elapsed() > Duration::from_secs(2) {
            app.screen = AppScreen::Chat;
        }

        if app.should_quit {
            break;
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);
    // Terminal cleanup is handled by TerminalGuard drop when _guard goes out
    // of scope here. No need for explicit cleanup.
    Ok(())
}

fn handle_mouse(app: &mut App, ev: MouseEvent) {
    if app.screen != AppScreen::Chat {
        return;
    }
    match ev.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        _ => {}
    }
}

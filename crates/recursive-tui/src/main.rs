//! Recursive TUI entry point.
//!
//! Initialises the terminal, spawns the agent backend worker, and
//! drives the event loop. All real logic lives in the library
//! modules (`recursive_tui::*`); this binary is intentionally tiny.

use std::io;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, MouseEvent, MouseEventKind};
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use recursive_tui::app::{App, AppScreen};
use recursive_tui::backend::Backend;
use recursive_tui::events::UserAction;
use recursive_tui::{keymap, ui};

#[tokio::main]
async fn main() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    // Capture mouse events so trackpad / wheel scroll drives the
    // transcript pane instead of the terminal's (empty) alt-screen
    // scrollback. Trade-off: text selection now requires holding
    // Option on macOS / Shift on most other terminals to fall back
    // to the terminal's own selection. This matches fake-cc and
    // Claude Code TUI behaviour.
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = Backend::spawn();
    let mut app = App::new();

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;
        // Advance the spinner one frame per draw tick (~50ms).
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
        }

        if app.screen == AppScreen::Splash && app.splash_start.elapsed() > Duration::from_secs(2) {
            app.screen = AppScreen::Chat;
        }

        if app.should_quit {
            break;
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);

    let _ = io::stdout().execute(DisableMouseCapture);
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

/// Map mouse / trackpad scroll events onto the transcript scroll
/// offset. We only react to wheel events; clicks and motion are
/// intentionally ignored (no clickable widgets yet).
///
/// Speed: 3 lines per wheel tick, matching what most terminals
/// emit per physical "notch" of a real wheel and what feels right
/// for two-finger trackpad scrolling on macOS.
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

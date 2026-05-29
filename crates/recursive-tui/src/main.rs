//! Recursive TUI entry point.
//!
//! Initialises the terminal, spawns the agent backend worker, and
//! drives the event loop. All real logic lives in the library
//! modules (`recursive_tui::*`); this binary is intentionally tiny.

use std::io;
use std::time::Duration;

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
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = Backend::spawn();
    let mut app = App::new();

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            if app.screen == AppScreen::Splash {
                                app.screen = AppScreen::Chat;
                            } else if let Some(action) = keymap::dispatch(&mut app, key.code) {
                                let _ = backend.action_tx.send(action);
                            }
                        }
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

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

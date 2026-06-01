pub mod app;
pub mod backend;
pub mod bash;
pub mod commands;
pub mod events;
pub mod keymap;
pub mod runtime_builder;
pub mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, MouseEvent, MouseEventKind};
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;

use crate::tui::app::{App, AppScreen};
use crate::tui::backend::Backend;
use crate::tui::events::UserAction;

/// Launch the TUI and run until the user quits.
pub async fn run() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = Backend::spawn();
    let mut app = App::new();

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
        }

        if app.screen == AppScreen::Splash
            && app.splash_start.elapsed() > Duration::from_secs(2)
        {
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

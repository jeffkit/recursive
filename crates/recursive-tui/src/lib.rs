//! `recursive-tui` — interactive terminal UI for the Recursive agent.
//!
//! This crate contains the full TUI implementation, physically separated from
//! the `recursive` core library. It depends only on `recursive` as a library
//! crate.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

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

// Re-export types used by embedders and by the binary entry point.
pub use cost::UsageStats;
pub use input_state::{InputMode, PromptInputState};
pub use model::{AppScreen, DiffHunk, DiffLine, DiffLineKind, TranscriptBlock};

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand as _;
use ratatui::prelude::*;
use ratatui::Terminal;

use crate::app::App;
use crate::backend::Backend;
use crate::events::UserAction;

// ── RAII guard ────────────────────────────────────────────────────────────────

/// Restores the terminal to its prior state on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Launch the TUI and run until the user quits.
pub async fn run() -> io::Result<()> {
    run_with_backend(Backend::spawn()).await
}

/// Launch the TUI with a pre-constructed [`Backend`].
///
/// Used by `--weixin` mode where the backend is created before the TUI
/// starts so the WeChat channel can be wired up.
pub async fn run_with_backend(backend: Backend) -> io::Result<()> {
    // Suppress global tracing output for the duration of the TUI.
    let _quiet_guard = recursive::logging::suppress_tracing_for_tui();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let _guard = RawModeGuard;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = backend;
    let mut app = App::new();
    app.permission_hook_enabled = backend.permission_enabled.clone();

    loop {
        terminal.draw(|frame| ui::chat::render(frame, &app))?;
        app.spinner_frame = app.spinner_frame.wrapping_add(1);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            if let Some(action) = keymap::dispatch(&mut app, key) {
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
            Some(perm_req) = backend.perm_rx.recv() => {
                app.set_pending_permission(perm_req);
            }
            Some(skill_ev) = backend.skill_install_rx.recv() => {
                use crate::events::SkillInstallEvent;
                match skill_ev {
                    SkillInstallEvent::Search(req) => {
                        app.handle_skill_search_request(req);
                    }
                    SkillInstallEvent::Files(req) => {
                        app.handle_skill_files_request(req);
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);
    Ok(())
}

/// Map trackpad / mouse wheel events onto the transcript scroll offset.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
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

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

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand as _;
use ratatui::prelude::*;
use ratatui::Terminal;

use crate::tui::app::App;
use crate::tui::backend::Backend;
use crate::tui::events::UserAction;

// ── RAII guard ────────────────────────────────────────────────────────────────

/// Restores the terminal to its prior state on drop.
///
/// The TUI runs full-screen on the terminal's alternate screen, so teardown
/// must leave the alternate screen, disable mouse capture, and restore cooked
/// mode. Doing this in `Drop` guarantees the terminal is restored even if the
/// event loop returns early with an error.
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
///
/// The TUI runs full-screen on the terminal's alternate screen. On exit the
/// alternate screen is left and the prior shell contents are restored, leaving
/// no banner or transcript residue in the scrollback.
pub async fn run() -> io::Result<()> {
    run_with_backend(Backend::spawn()).await
}

/// Launch the TUI with a pre-constructed [`Backend`].
///
/// Used by `--weixin` mode where the backend is created before the TUI
/// starts so the WeChat channel can be wired up.
pub async fn run_with_backend(backend: Backend) -> io::Result<()> {
    // Suppress global tracing output for the duration of the TUI.
    let _quiet_guard = crate::logging::suppress_tracing_for_tui();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;
    let _guard = RawModeGuard;

    // Full-screen viewport: ratatui's default `Terminal::new` uses
    // `Viewport::Fullscreen`, which owns the whole alternate screen and
    // autoresizes on every `draw()` — no manual size polling or viewport
    // rebuild needed.
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
                use crate::tui::events::SkillInstallEvent;
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
/// 3 lines per tick matches macOS trackpad feel and real-wheel notch speed.
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

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

/// In-process test harness — the AI's "eyes" for TUI testing.
///
/// Test-only: drives `App` + keymap + `handle_ui_event` and renders to an
/// offscreen `ratatui::Buffer` via `TestBackend`. See the module docs for
/// the observation / effectiveness loops it enables.
#[cfg(test)]
pub mod harness;

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

static PANIC_HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();

/// Install a panic hook that keeps panic output off the TUI surface.
///
/// The default panic hook writes directly to fd 2 — the same surface the
/// alternate screen is rendered onto. A panic inside a tool task is
/// caught by the runtime and surfaced to the agent as
/// "ERROR: tool task panicked during parallel execution", but the raw
/// Rust panic text is still dumped on top of the TUI by the default
/// hook. Because that text is not part of ratatui's diff buffer, no
/// redraw ever erases it, so it sticks around the input box until the
/// user resizes the terminal or runs `reset`.
///
/// While the TUI is active (`is_tui_quiet()` is true) we instead append
/// the panic message to `<user_data_dir>/logs/tui-panic.log` and leave
/// the screen untouched. When the TUI is not active, the previous
/// (default) hook runs unchanged so panics still print normally in CLI
/// runs and in tests. Installed at most once per process.
fn install_tui_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !recursive::logging::is_tui_quiet() {
                previous(info);
                return;
            }
            let payload = info.payload();
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown location>".to_string());
            let thread = std::thread::current()
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unnamed>".to_string());
            let _ = append_panic_log(&thread, &location, &msg);
        }));
    });
}

/// Append a captured panic to `<user_data_dir>/logs/tui-panic.log`.
fn append_panic_log(thread: &str, location: &str, msg: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = recursive::paths::user_data_dir().join("logs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("tui-panic.log");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let content = format!("[unix_ts={now}] thread '{thread}' panicked at {location}:\n{msg}\n\n");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// Launch the TUI and run until the user quits.
pub async fn run() -> io::Result<()> {
    run_with_backend(Backend::spawn()).await
}

/// Launch the TUI with a pre-constructed [`Backend`].
///
/// Used by `--weixin` mode where the backend is created before the TUI
/// starts so the WeChat channel can be wired up.
pub async fn run_with_backend(backend: Backend) -> io::Result<()> {
    // Route panics to a log file (not stderr) while the TUI owns the
    // terminal, so a panicking tool task can't dump raw text onto the
    // alternate screen where no redraw can clear it.
    install_tui_panic_hook();

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
    // Share the backend's session-mutable sandbox roots so `/add-dir` grants
    // the agent runtime access to directories outside the workspace.
    app.session_roots = backend.session_roots.clone();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_panic_log_writes_under_recursive_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(tmp.path());
        append_panic_log(
            "worker",
            "src/x.rs:42:13",
            "byte index 79 is not a char boundary",
        )
        .expect("append");
        let log = std::fs::read_to_string(
            recursive::paths::user_data_dir()
                .join("logs")
                .join("tui-panic.log"),
        )
        .expect("read log");
        assert!(log.contains("thread 'worker'"), "missing thread: {log}");
        assert!(log.contains("src/x.rs:42:13"), "missing location: {log}");
        assert!(
            log.contains("byte index 79 is not a char boundary"),
            "missing message: {log}"
        );
    }
}

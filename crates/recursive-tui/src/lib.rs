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
pub mod ollama_probe;
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
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
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
    // `drop -> ()` only suppresses crossterm terminal commands (disable raw
    // mode, leave alternate screen, disable mouse capture) whose effects land
    // on the real terminal and are not observable from a unit test. Skip
    // mutation of the drop body.
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        let _ = io::stdout().execute(DisableBracketedPaste);
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
    io::stdout().execute(EnableBracketedPaste)?;
    let _guard = RawModeGuard;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut backend = backend;
    let mut app = App::new();
    app.permission_hook_enabled = backend.permission_enabled.clone();
    // Share the backend's session-mutable sandbox roots so `/add-dir` grants
    // the agent runtime access to directories outside the workspace.
    app.session_roots = backend.session_roots.clone();

    loop {
        terminal.draw(|frame| ui::chat::render(frame, &mut app))?;
        app.spinner_frame = app.spinner_frame.wrapping_add(1);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            // Guard against fragmented SGR mouse sequences.
                            //
                            // The terminal can split `\x1b[<btn;col;rowM` at the
                            // first byte, delivering `\x1b` as a standalone byte
                            // before the rest. crossterm then emits `KeyCode::Esc`
                            // for the `\x1b` and the remaining `[<btn;col;rowM`
                            // arrives as individual key chars on the next poll.
                            //
                            // Detection: if ESC arrives and the very next pending
                            // event (available with zero timeout) is `[`, we are
                            // almost certainly seeing a fragmented sequence — real
                            // ESC + `[` from a human typist would never arrive
                            // that quickly. Discard both and let the rest of the
                            // sequence be silently consumed in subsequent iterations
                            // (the chars are harmless inserts; the double-press
                            // guard in handle_esc prevents an interrupt).
                            if key.code == KeyCode::Esc
                                && event::poll(Duration::ZERO)?
                            {
                                if let Ok(Event::Key(nk)) = event::read() {
                                    if nk.kind == KeyEventKind::Press
                                        && nk.code == KeyCode::Char('[')
                                    {
                                        // Fragmented mouse sequence confirmed —
                                        // drop the ESC and the `[`; the
                                        // remaining sequence bytes will be read
                                        // in the next iteration and inserted
                                        // harmlessly into the buffer (or ignored
                                        // when the user is not in a text-input
                                        // context).
                                        continue;
                                    }
                                    // Next event was something else — process ESC
                                    // normally, then handle that event.
                                    if let Some(action) = keymap::dispatch(&mut app, key) {
                                        let _ = backend.action_tx.send(action);
                                    }
                                    if nk.kind == KeyEventKind::Press {
                                        if let Some(action) = keymap::dispatch(&mut app, nk) {
                                            let _ = backend.action_tx.send(action);
                                        }
                                    }
                                    continue;
                                }
                                // read() failed — fall through to normal dispatch.
                            }
                            if let Some(action) = keymap::dispatch(&mut app, key) {
                                let _ = backend.action_tx.send(action);
                            }
                        }
                        Event::Mouse(mev) => handle_mouse(&mut app, mev),
                        Event::Paste(text) => app.handle_paste(&text),
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

/// Map trackpad / mouse wheel events onto the transcript scroll offset, and
/// left-button press-drag-release onto text selection over the visible
/// transcript window (Goal-349).
///
/// Scroll behaviour is unchanged from before (same ±3 rows per wheel tick)
/// except for one side effect: any scroll clears the active selection, whose
/// indices are relative to the visible window — scrolling moves the window
/// under the highlight, so a stale selection would point at the wrong rows.
///
/// Selection relies on the `Down`→`Drag`→`Up` pairing crossterm delivers for
/// a single button: `Down(Left)` starts a selection at the clicked row,
/// `Drag(Left)` extends it (only when a selection is already active, so a
/// stray drag from another button is ignored), and `Up(Left)` copies the
/// selected rows to the clipboard and clears the highlight.
fn handle_mouse(app: &mut App, ev: MouseEvent) {
    match ev.kind {
        MouseEventKind::ScrollUp => {
            app.selection = None; // Goal-349: clear-on-scroll invariant
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        MouseEventKind::ScrollDown => {
            app.selection = None; // Goal-349: clear-on-scroll invariant
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Begin a selection at the clicked visible row. The messages
            // panel is the top layout chunk, so the terminal row equals the
            // visible-window row index.
            app.selection = Some((ev.row as usize, ev.row as usize));
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Extend the selection to the dragged-to row. Only extends an
            // existing selection; a drag without a prior Down (e.g. another
            // button held) is ignored.
            if let Some((start, _)) = app.selection {
                let end = ev.row as usize;
                app.selection = Some((start.min(end), start.max(end)));
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // Release: copy the selected rows' text and clear.
            if let Some((start, end)) = app.selection {
                copy_visible_rows(app, start, end);
                app.selection = None;
            }
        }
        _ => {}
    }
}

/// Goal-349: copy the inclusive range `[start, end]` of *visible* rows to
/// the system clipboard and to `app.last_copied` (test mirror).
///
/// The rows come from [`crate::ui::chat::visible_physical_rows`] at the last
/// rendered width — the same rows the render path paints — so the copied
/// text always matches the selection highlight.
fn copy_visible_rows(app: &mut App, start: usize, end: usize) {
    let rows = crate::ui::chat::visible_physical_rows(app, app.last_render_width);
    let lo = start.min(rows.len());
    let hi = (end + 1).min(rows.len());
    let text: String = rows[lo..hi]
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    app.copy_text(text);
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

    // ── debt tests (2026-07-02) ───────────────────────────────────────────

    use crossterm::event::KeyModifiers;

    #[test]
    fn handle_mouse_scroll_up_increases_offset() {
        // kills delete ScrollUp arm (215:9).
        let mut app = App::new();
        app.scroll_offset = 0;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_offset, 3);
    }

    #[test]
    fn handle_mouse_scroll_down_decreases_offset() {
        // kills delete ScrollDown arm (218:9).
        let mut app = App::new();
        app.scroll_offset = 5;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_offset, 2);
    }

    #[test]
    fn install_tui_panic_hook_writes_log_when_quiet() {
        // With is_tui_quiet=true, a panic must be appended to
        // tui-panic.log (not printed via the default hook).
        // kills install_tui_panic_hook -> () (87:5: hook never installed ->
        //   default hook runs -> no log file) and delete `!` (90:16: guard
        //   flips to `if is_tui_quiet()` -> the mutant calls the previous
        //   (default) hook instead of writing the log).
        let tmp = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(tmp.path());
        let _quiet = recursive::logging::suppress_tracing_for_tui();

        install_tui_panic_hook();
        let result = std::panic::catch_unwind(|| {
            panic!("debt-test-panic-marker-XYZ");
        });
        assert!(result.is_err(), "expected the panic to be caught");

        // Restore the default hook so later tests aren't affected.
        let _installed = std::panic::take_hook();

        let log_path = recursive::paths::user_data_dir()
            .join("logs")
            .join("tui-panic.log");
        let log = std::fs::read_to_string(&log_path).unwrap_or_else(|_| String::new());
        assert!(
            log.contains("debt-test-panic-marker-XYZ"),
            "expected panic marker in log; got: {log}"
        );
    }

    // ── Goal-349: mouse-drag select & copy ──────────────────────────────

    use crate::events::UiEvent;
    use crate::harness::{Harness, Screen};
    use ratatui::style::Modifier;

    fn mouse_event(kind: MouseEventKind, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// True when any cell on screen row `y` carries the REVERSED modifier —
    /// the selection highlight the renderer paints. Asserting on the
    /// specific modifier (not "any bg") is unambiguous even for rows whose
    /// existing style already sets a background.
    fn row_has_reversed(screen: &Screen, y: u16) -> bool {
        (0..screen.width()).any(|x| screen.style(x, y).add_modifier.contains(Modifier::REVERSED))
    }

    #[test]
    fn mouse_down_drag_selects_row_range() {
        // Down at row 0 then Drag to row 2 must select the inclusive range
        // (0, 2) and the renderer must reverse-highlight exactly those rows.
        // (Blank-line-separated content — the markdown renderer joins
        // single-newline paragraphs into one row.)
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "line one\n\nline two\n\nline three".into(),
        });
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Down(MouseButton::Left), 0),
        );
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 2),
        );
        assert_eq!(h.app().selection, Some((0, 2)));

        let screen = h.render();
        for y in 0..=2 {
            assert!(
                row_has_reversed(&screen, y),
                "row {y} should be REVERSED after drag-select\n{}",
                screen.numbered()
            );
        }
        // The trailing blank row (window row 3) must NOT be reversed —
        // selection covers exactly the dragged content rows.
        assert!(
            !row_has_reversed(&screen, 3),
            "blank row 3 must not be reversed\n{}",
            screen.numbered()
        );
    }

    #[test]
    fn mouse_up_copies_selection_and_clears() {
        // Down(0) → Drag(1) → Up(1): the selected visible rows' rendered
        // text lands in `last_copied` (the clipboard mirror; the live
        // clipboard is not assertable headless) and the selection clears.
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "alpha\n\nbeta".into(),
        });
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Down(MouseButton::Left), 0),
        );
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 1),
        );
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Up(MouseButton::Left), 1),
        );
        assert_eq!(
            h.app().last_copied.as_deref(),
            Some("•  alpha\n  beta"),
            "release must copy the selected visible rows (rendered text)"
        );
        assert!(
            h.app().selection.is_none(),
            "selection must clear after a copy on release"
        );
    }

    #[test]
    fn mouse_drag_without_prior_down_is_ignored() {
        // A stray Drag (e.g. another button held) must not start a selection
        // on its own — only a Down → Drag → Up pairing selects.
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "a\nb\nc".into(),
        });
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 2),
        );
        assert_eq!(
            h.app().selection,
            None,
            "Drag without a prior Down must not create a selection"
        );
        let screen = h.render();
        assert!(
            !row_has_reversed(&screen, 2),
            "row 2 must not be highlighted without a Down first"
        );
    }

    #[test]
    fn mouse_wheel_scroll_clears_selection() {
        // Pins the clear-on-scroll invariant for the mouse path: scrolling
        // moves the window under the selection, so the highlight must not
        // stay pointing at now-wrong rows.
        let mut h = Harness::new();
        h.pump(UiEvent::AssistantMessage {
            content: "a\nb\nc\nd".into(),
        });
        handle_mouse(
            h.app_mut(),
            mouse_event(MouseEventKind::Down(MouseButton::Left), 0),
        );
        assert!(h.app().selection.is_some());
        handle_mouse(h.app_mut(), mouse_event(MouseEventKind::ScrollUp, 0));
        assert!(h.app().selection.is_none(), "scroll must clear selection");
        assert_eq!(
            h.app().scroll_offset,
            3,
            "scroll wheel behaviour itself is unchanged"
        );
    }
}

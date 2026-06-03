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

use std::io::{self, Write as _};
use std::time::Duration;

use unicode_width::UnicodeWidthStr as _;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::tui::app::App;
use crate::tui::backend::Backend;
use crate::tui::events::UserAction;

// ── Startup banner ────────────────────────────────────────────────────────────

/// Normal inline viewport height (input + status bar + in-flight streaming).
/// Completed messages are pushed to the terminal's native scrollback via
/// `terminal.insert_before()` so they remain readable above this region.
const INLINE_HEIGHT_NORMAL: u16 = 10;

/// Expanded inline viewport height used while a modal or permission popup is
/// visible.  70% of 40 ≈ 28 rows — plenty for Help, ResumePicker, etc.
/// When the modal closes the viewport shrinks back to `INLINE_HEIGHT_NORMAL`.
const INLINE_HEIGHT_EXPANDED: u16 = 40;

/// Print the compact logo, version, model, and recent sessions to
/// stdout before the inline TUI viewport starts. Styled after the
/// fake-cc welcome screen: a small 3-line logo, a dot-separator, and
/// a brief session list — all flowing into the terminal's scrollback
/// so they remain readable above the interactive area.
fn print_startup_banner(workspace: &std::path::Path) {
    const CYAN: &str = "\x1b[36m";
    const BOLD: &str = "\x1b[1m";
    const DARK_GRAY: &str = "\x1b[90m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    // 3-line compact logo (fits in ~48 cols)
    println!("{CYAN}{BOLD}╦═╗╔═╗╔═╗╦ ╦╦═╗╔═╗╦╦  ╦╔═╗{RESET}");
    println!("{CYAN}{BOLD}╠╦╝║╣ ║  ║ ║╠╦╝╚═╗║╚╗╔╝║╣ {RESET}");
    println!("{CYAN}{BOLD}╩╚═╚═╝╚═╝╚═╝╩╚═╚═╝╩ ╚╝ ╚═╝{RESET}");

    // Version + model on one line (fake-cc style).
    // Use the same detection logic as the status bar so the value is always real.
    let model = crate::tui::cost::detect_model_name();
    println!("{DIM}  v{}  ·  {model}{RESET}", env!("CARGO_PKG_VERSION"));

    // Dot separator (fake-cc style)
    println!("{DARK_GRAY}  ……………………………………………………………………………………………………{RESET}");

    // Recent user-initiated sessions — show newest first, skip self-improve runs
    // (those have a `goal` but no `last_prompt`; TUI sessions set `last_prompt`).
    let mut shown = 0;
    if let Ok(mut sessions) = crate::session::SessionReader::list_sessions(workspace) {
        // list_sessions returns alphabetical (oldest first); reverse for newest first.
        sessions.reverse();
        for dir in &sessions {
            if shown >= 3 {
                break;
            }
            if let Ok(meta) = crate::session::SessionReader::load_meta(dir) {
                // Only show sessions that have a human prompt (not internal goal runs)
                if let Some(ref prompt) = meta.last_prompt {
                    let short: String = prompt.chars().take(60).collect();
                    let ellipsis = if prompt.chars().count() > 60 {
                        "…"
                    } else {
                        ""
                    };
                    println!("{DARK_GRAY}  › {short}{ellipsis}{RESET}");
                    shown += 1;
                }
            }
        }
    }

    println!();
    let _ = io::stdout().flush();
}

// ── RAII guard ────────────────────────────────────────────────────────────────

/// Restores the terminal to cooked mode on drop.
///
/// Because we use `Viewport::Inline` (no alternate screen), we only
/// need to disable raw mode and emit a final newline so the shell
/// prompt appears on a fresh line below the last rendered frame.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = writeln!(io::stdout());
    }
}

// ── Terminal factory ──────────────────────────────────────────────────────────

/// Create a new inline-mode terminal with the given viewport height.
///
/// The cursor position at call-time determines where the viewport is anchored.
/// Passing a larger `height` makes ratatui scroll the terminal up to create
/// room (pushing earlier content into the scrollback), matching the fake-cc
/// behaviour of expanding the input area for popups and modals.
fn make_inline_terminal(
    height: u16,
) -> io::Result<ratatui::Terminal<CrosstermBackend<io::Stdout>>> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Launch the TUI and run until the user quits.
///
/// Uses `Viewport::Inline` so the TUI occupies a fixed-height region at
/// the bottom of the terminal's main scrollback buffer instead of
/// switching to an alternate screen. The startup banner (logo + recent
/// sessions) is printed to stdout before the TUI starts and remains
/// visible in the scrollback above the viewport.
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

    // Determine workspace for session listing in the banner.
    let workspace = crate::config::Config::from_env()
        .map(|c| c.workspace)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Print the startup banner before raw mode so scrollback is intact.
    print_startup_banner(&workspace);

    enable_raw_mode()?;
    let _guard = RawModeGuard;

    let mut terminal = make_inline_terminal(INLINE_HEIGHT_NORMAL)?;
    let mut current_inline_height = INLINE_HEIGHT_NORMAL;

    let mut backend = backend;
    let mut app = App::new();
    app.permission_hook_enabled = backend.permission_enabled.clone();

    loop {
        // ── Progressive output: flush completed blocks to scrollback ──────
        // Advance `last_printed_idx` over any newly-finalized blocks and
        // push their rendered lines into `print_queue`.
        app.flush_ready_blocks();

        // Drain the queue: each batch of lines is inserted *above* the
        // inline viewport so it scrolls into the terminal's native
        // scrollback buffer.  We render into a ratatui `Buffer` via the
        // `Paragraph` widget so all existing ANSI styling is preserved.
        let queued: Vec<Vec<Line<'static>>> = app.print_queue.drain(..).collect();
        for lines in queued {
            let h = (lines.len() as u16).max(1);
            terminal.insert_before(h, |buf| {
                let area = buf.area;
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .render(area, buf);
                // Fix: ratatui's `draw_lines` (used by insert_before) writes every
                // buffer cell individually including the continuation cells that
                // follow wide (CJK/emoji) characters.  Those cells are initialised
                // to Cell::EMPTY whose symbol is " " (space), so crossterm prints a
                // visible space after each wide character.  Setting them to "" makes
                // Print("") a no-op and eliminates the inter-character gaps.
                let mut i = 0;
                while i < buf.content.len() {
                    let w = buf.content[i].symbol().width();
                    if w >= 2 {
                        for j in 1..w {
                            if i + j < buf.content.len() {
                                buf.content[i + j].set_symbol("");
                            }
                        }
                    }
                    i += 1;
                }
            })?;
        }

        // ── Dynamic viewport height (fake-cc style) ───────────────────────
        // Expand the viewport when modals / permission popups are open so
        // they have room to render.  Shrink back once everything is closed.
        let needs_expanded = !app.modals.is_empty() || app.pending_permission.is_some();
        let desired_height = if needs_expanded {
            INLINE_HEIGHT_EXPANDED
        } else {
            INLINE_HEIGHT_NORMAL
        };
        if desired_height != current_inline_height {
            terminal = make_inline_terminal(desired_height)?;
            current_inline_height = desired_height;
        }

        terminal.draw(|frame| ui::chat::render(frame, &app))?;
        app.spinner_frame = app.spinner_frame.wrapping_add(1);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press {
                            if let Some(action) = keymap::dispatch(&mut app, key) {
                                let _ = backend.action_tx.send(action);
                            }
                        }
                    }
                }
            }
            Some(ui_event) = backend.event_rx.recv() => {
                app.handle_ui_event(ui_event);
            }
            Some(perm_req) = backend.perm_rx.recv() => {
                app.set_pending_permission(perm_req);
            }
        }

        if app.should_quit {
            break;
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);
    Ok(())
}

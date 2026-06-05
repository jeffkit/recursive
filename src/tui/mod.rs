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

/// Minimum viewport height used as a fallback when the terminal size cannot
/// be queried. The actual viewport is sized to the full terminal height so
/// old shell history is pushed into native scrollback and the TUI fills the
/// entire visible area.
const MIN_INLINE_HEIGHT: u16 = 20;

/// How many rendered lines to keep in the in-viewport rolling buffer
/// (`app.recent_display`).  Lines beyond this cap are pushed to native
/// scrollback via `insert_before()` and removed from `recent_display`,
/// so every line lives in exactly one place (no duplicates).
///
/// Set large (300) so the startup banner and most recent conversation
/// history stay accessible via in-app scroll (Shift+↑) for the entire
/// session. Native scrollback (`terminal.insert_before`) is still used
/// for truly long transcripts.
const RECENT_DISPLAY_MAX: usize = 300;

/// Build the startup banner as ratatui `Line`s for display inside the
/// viewport's messages panel.
///
/// Rendering the banner inside `recent_display` (rather than printing to
/// stdout) means it participates in the same information flow as chat
/// messages: new messages naturally push the banner upward within the
/// viewport, matching the intended UX where logo/sessions appear directly
/// above the input box and scroll off as conversation grows.
fn make_viewport_banner(workspace: &std::path::Path) -> Vec<Line<'static>> {
    use ratatui::style::Modifier;

    let cyan_bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let dark_gray = Style::default().fg(Color::DarkGray);
    let white_bold = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(Color::DarkGray);

    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(60);
    // 40/60 split: left holds logo + meta, right holds session list.
    // Mirrors the layout from the old stdout two-column banner.
    let left_col = (term_width * 40 / 100).max(30);
    // " │ " separator = 3 chars, "  › " prefix = 4 chars.
    let max_session_chars = term_width.saturating_sub(left_col + 7);

    let model = crate::tui::cost::detect_model_name();
    let version = env!("CARGO_PKG_VERSION");

    let ws_str = workspace.display().to_string();
    let home_str = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let ws_display: String = if !home_str.is_empty() && ws_str.starts_with(&home_str) {
        format!("~{}", &ws_str[home_str.len()..])
    } else {
        ws_str
    };

    // ── Left column: logo rows + version / workspace ──────────────────────
    // Each entry is (padded_text, style). Text is padded to `left_col`
    // visible columns so the separator always aligns on every row.
    // Box-drawing characters and ASCII are all single-width, so Rust's
    // char-count padding `{:<N$}` equals visual-column padding here.
    let left_lines: Vec<(String, Style)> = vec![
        (
            format!("{:<width$}", "╦═╗╔═╗╔═╗╦ ╦╦═╗╔═╗╦╦  ╦╔═╗", width = left_col),
            cyan_bold,
        ),
        (
            format!("{:<width$}", "╠╦╝║╣ ║  ║ ║╠╦╝╚═╗║╚╗╔╝║╣ ", width = left_col),
            cyan_bold,
        ),
        (
            format!("{:<width$}", "╩╚═╚═╝╚═╝╚═╝╩╚═╚═╝╩ ╚╝ ╚═╝", width = left_col),
            cyan_bold,
        ),
        (format!("{:<left_col$}", ""), Style::default()),
        (
            format!(
                "{:<width$}",
                format!("  v{version}  ·  {model}"),
                width = left_col
            ),
            dim,
        ),
        (
            format!("{:<width$}", format!("  {ws_display}"), width = left_col),
            dark_gray,
        ),
    ];

    // ── Right column: session list ────────────────────────────────────────
    let mut right_lines: Vec<(String, Style)> =
        vec![("Recent sessions".to_string(), white_bold)];

    match crate::session::SessionReader::list_sessions_sorted_by_updated_at(workspace) {
        Ok(sorted) if !sorted.is_empty() => {
            for (_, meta) in sorted.iter().take(5) {
                let label = meta
                    .name
                    .as_deref()
                    .or(meta.last_prompt.as_deref())
                    .unwrap_or(meta.goal.as_str());
                let short: String = label.chars().take(max_session_chars).collect();
                let ellipsis = if label.chars().count() > max_session_chars {
                    "…"
                } else {
                    ""
                };
                right_lines.push((format!("  › {short}{ellipsis}"), dark_gray));
            }
        }
        _ => {
            right_lines.push(("  No recent sessions".to_string(), dark_gray));
        }
    }

    // ── Merge into two-column ratatui Lines ───────────────────────────────
    let num_rows = left_lines.len().max(right_lines.len());
    let empty_left = format!("{:<left_col$}", "");
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(num_rows + 1);

    for i in 0..num_rows {
        let mut spans: Vec<Span<'static>> = Vec::new();

        if let Some((text, style)) = left_lines.get(i) {
            spans.push(Span::styled(text.clone(), *style));
        } else {
            spans.push(Span::raw(empty_left.clone()));
        }

        // Separator + right content only when the right column has a row.
        if let Some((text, style)) = right_lines.get(i) {
            spans.push(Span::styled(" │ ", sep_style));
            spans.push(Span::styled(text.clone(), *style));
        }

        lines.push(Line::from(spans));
    }

    lines.push(Line::raw(""));
    lines
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

    enable_raw_mode()?;
    let _guard = RawModeGuard;

    // Size the viewport to the full terminal height so old shell history is
    // pushed into native scrollback and the TUI fills the entire visible area.
    // With the banner rendered inside `recent_display` (not stdout) and content
    // bottom-aligned, a full-height viewport has no duplicate or blank-space
    // issues that affected earlier fixed-height attempts.
    let mut last_size: (u16, u16) = crossterm::terminal::size().unwrap_or((100, MIN_INLINE_HEIGHT));
    let mut terminal = make_inline_terminal(last_size.1.max(MIN_INLINE_HEIGHT))?;

    let mut backend = backend;
    let mut app = App::new();
    app.permission_hook_enabled = backend.permission_enabled.clone();

    // Seed the viewport's message panel with the startup banner.
    // Placing the banner inside `recent_display` (rather than printing to
    // stdout) means new messages naturally push it upward and eventually
    // off screen — exactly the "logo is part of the information flow" UX.
    app.recent_display = make_viewport_banner(&workspace);

    // Track the modal stack depth at the end of each event cycle so the next
    // iteration can detect a dismissal and pre-draw before `insert_before`.
    //
    // Root-cause of the modal residue bug:
    //   `insert_before()` scrolls the TOP rows of the current inline viewport
    //   into the terminal's native scrollback buffer. Those rows reflect the
    //   LAST call to `terminal.draw()`. When the user presses Esc to close a
    //   modal, the dismissal is processed in the EVENT phase (end of iteration
    //   N), but `terminal.draw()` for iteration N already ran *before* the
    //   event — so the viewport still shows the modal. If the NEXT iteration
    //   (N+1) calls `insert_before()` before `terminal.draw()`, it pushes the
    //   stale modal frame into scrollback, creating a permanent ghost.
    //
    // Fix: if the modal count dropped since last iteration AND there are blocks
    //   to push, do a preliminary `terminal.draw()` first to refresh the
    //   terminal with the modal-free state before `insert_before()` runs.
    let mut last_modal_count: usize = 0;

    loop {
        // ── Progressive output: flush completed blocks to scrollback ──────
        // Advance `last_printed_idx` over any newly-finalized blocks and
        // push their rendered lines into `print_queue`.
        app.flush_ready_blocks(last_size.0);

        // Drain the queue: sliding-window approach — each line lives in
        // exactly ONE place, so there is never duplicate content.
        //
        // Strategy:
        //   1. Append the new lines to `recent_display` (the viewport's
        //      messages panel).
        //   2. If `recent_display` now exceeds RECENT_DISPLAY_MAX, drain the
        //      OLDEST lines and push them to native scrollback via
        //      `insert_before()`.  Those drained lines are removed from
        //      `recent_display`, so they exist only in the scrollback.
        //
        // Result:
        //   • Viewport always shows the last RECENT_DISPLAY_MAX rendered lines
        //     → the messages panel is never blank between turns.
        //   • As the conversation grows past RECENT_DISPLAY_MAX lines, older
        //     content flows to native scrollback (pushing the startup banner
        //     upward) and is accessible via terminal scroll (Shift+PgUp).
        //   • No line is ever simultaneously in the viewport and in native
        //     scrollback, so duplicate display is structurally impossible.
        let queued: Vec<Vec<Line<'static>>> = app.print_queue.drain(..).collect();

        // Pre-draw: if a modal was dismissed in the previous event cycle AND
        // there are blocks queued to process, we must redraw now so the
        // terminal reflects the modal-free state before any insert_before call.
        if app.modals.len() < last_modal_count && !queued.is_empty() {
            terminal.draw(|frame| ui::chat::render(frame, &app))?;
        }

        for lines in queued {
            // 1. Append to the viewport rolling buffer.
            app.recent_display.extend(lines);

            // 2. Overflow → push excess to native scrollback.
            if app.recent_display.len() > RECENT_DISPLAY_MAX {
                let drain = app.recent_display.len() - RECENT_DISPLAY_MAX;
                let overflow: Vec<Line<'static>> = app.recent_display.drain(..drain).collect();
                let h = (overflow.len() as u16).max(1);
                terminal.insert_before(h, |buf| {
                    let area = buf.area;
                    Paragraph::new(overflow)
                        .wrap(Wrap { trim: false })
                        .render(area, buf);
                    // Fix wide-char continuation cells: ratatui's draw_lines
                    // initialises them to Cell::EMPTY (symbol=" "), causing a
                    // visible space after each wide (CJK/emoji) character.
                    // Setting them to "" makes Print("") a no-op.
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
                // Detect terminal resize: rebuild the inline viewport so
                // chat / status / input span the new full width instead
                // of staying locked to the size at startup.
                if let Ok(cur) = crossterm::terminal::size() {
                    if cur != last_size {
                        last_size = cur;
                        terminal = make_inline_terminal(cur.1.max(MIN_INLINE_HEIGHT))?;
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

        // Record the modal count AFTER events so the next iteration can detect
        // any dismissal that happened during this cycle's event handling.
        last_modal_count = app.modals.len();

        if app.should_quit {
            break;
        }
    }

    let _ = backend.action_tx.send(UserAction::Shutdown);
    Ok(())
}

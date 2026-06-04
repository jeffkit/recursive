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

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

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

/// Strip ANSI escape sequences from a string and return the visible character count.
fn visible_len(s: &str) -> usize {
    let mut count = 0;
    let mut in_escape = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_escape {
            if ch == 'm'
                || ch == 'K'
                || ch == 'H'
                || ch == 'J'
                || ch == 'A'
                || ch == 'B'
                || ch == 'C'
                || ch == 'D'
                || ch == 'G'
            {
                in_escape = false;
            }
        } else if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            in_escape = true;
        } else {
            count += ch.width().unwrap_or(1);
        }
    }
    count
}

/// Pad a styled string (which may contain ANSI codes) to `width` visible columns.
fn pad_to(s: &str, width: usize, reset: &str) -> String {
    let vlen = visible_len(s);
    if vlen >= width {
        s.to_string()
    } else {
        format!("{s}{reset}{}", " ".repeat(width - vlen))
    }
}

/// Compute (left_col, right_col) for the banner at the given terminal width.
///
/// Splits the terminal at a fixed 40/60 ratio so the right "Recent sessions"
/// column gets a comfortable width even on wide terminals. The left column
/// is floored at 30 so the 28-char logo + 2 indent always fits.
fn compute_column_widths(term_width: usize) -> (usize, usize) {
    let left_col = (term_width * 40 / 100).max(30);
    let right_col = term_width.saturating_sub(left_col + 1);
    (left_col, right_col)
}

/// Print the startup banner in Claude Code style: two-column layout.
///
/// Left column  — logo (3 lines), version · model, workspace path.
/// Right column — "Recent sessions" header + newest-first session list.
///
/// Both columns flow into the terminal's native scrollback so they
/// remain readable above the interactive inline viewport.
fn print_startup_banner(workspace: &std::path::Path) {
    const CYAN: &str = "\x1b[36m";
    const BOLD: &str = "\x1b[1m";
    const DARK_GRAY: &str = "\x1b[90m";
    const DIM: &str = "\x1b[2m";
    const WHITE: &str = "\x1b[97m";
    const RESET: &str = "\x1b[0m";

    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
        .max(60);

    // 40/60 split: left col holds the logo + meta, right col holds the
    // session list. The split scales with terminal width so wide
    // terminals don't leave 100+ chars of empty space on the right.
    let (left_col, right_col) = compute_column_widths(term_width);

    // ── Left column lines ─────────────────────────────────────────────
    let model = crate::tui::cost::detect_model_name();
    let version = env!("CARGO_PKG_VERSION");

    let ws_str = workspace.display().to_string();
    let home_str = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let ws_display = if !home_str.is_empty() && ws_str.starts_with(&home_str) {
        format!("~{}", &ws_str[home_str.len()..])
    } else {
        ws_str
    };

    let left_lines: Vec<String> = vec![
        format!("{CYAN}{BOLD}╦═╗╔═╗╔═╗╦ ╦╦═╗╔═╗╦╦  ╦╔═╗{RESET}"),
        format!("{CYAN}{BOLD}╠╦╝║╣ ║  ║ ║╠╦╝╚═╗║╚╗╔╝║╣ {RESET}"),
        format!("{CYAN}{BOLD}╩╚═╚═╝╚═╝╚═╝╩╚═╚═╝╩ ╚╝ ╚═╝{RESET}"),
        String::new(),
        format!("{DIM}  v{version}  ·  {model}{RESET}"),
        format!("{DARK_GRAY}  {ws_display}{RESET}"),
    ];

    // ── Right column lines ────────────────────────────────────────────
    let max_prompt_chars = right_col.saturating_sub(4); // "  › " = 4 chars

    let mut session_lines: Vec<String> = Vec::new();
    // Use sorted-by-updated_at so we get real recency ordering.
    if let Ok(sorted) = crate::session::SessionReader::list_sessions_sorted_by_updated_at(workspace)
    {
        // list_sessions_sorted_by_updated_at returns newest first.
        // Show all sessions (TUI, CLI, self-improve) with display priority:
        // name > last_prompt > goal.
        for (_, meta) in sorted.iter().take(5) {
            let label = meta
                .name
                .as_deref()
                .or(meta.last_prompt.as_deref())
                .unwrap_or(meta.goal.as_str());
            let short: String = label.chars().take(max_prompt_chars).collect();
            let ellipsis = if label.chars().count() > max_prompt_chars {
                "…"
            } else {
                ""
            };
            session_lines.push(format!("{DARK_GRAY}  › {short}{ellipsis}{RESET}"));
        }
    }

    let mut right_lines: Vec<String> = vec![format!("{WHITE}{BOLD}Recent sessions{RESET}")];
    if session_lines.is_empty() {
        right_lines.push(format!("{DARK_GRAY}  No recent sessions{RESET}"));
    } else {
        right_lines.extend(session_lines);
    }

    // ── Render side-by-side ───────────────────────────────────────────
    let num_rows = left_lines.len().max(right_lines.len());
    let sep = format!("{DARK_GRAY}│{RESET}");

    for i in 0..num_rows {
        let left = left_lines.get(i).map(String::as_str).unwrap_or("");
        let right = right_lines.get(i).map(String::as_str).unwrap_or("");

        // Pad left to `left_col` visible columns then append separator and right.
        let padded = pad_to(left, left_col, RESET);
        if right.is_empty() {
            println!("{padded}");
        } else {
            println!("{padded}{sep} {right}");
        }
    }

    // Add a blank line before the TUI viewport starts.
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
    // Track the terminal size so we can rebuild the inline viewport when
    // the user resizes the window. Crossterm's `Event::Resize` is not
    // reliably delivered across all terminals, so we poll `terminal::size`
    // and rebuild whenever it changes.
    let mut last_size: (u16, u16) = crossterm::terminal::size().unwrap_or((100, 30));

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
                // Detect terminal resize: rebuild the inline viewport so
                // chat / status / input span the new full width instead
                // of staying locked to the size at startup.
                if let Ok(cur) = crossterm::terminal::size() {
                    if cur != last_size {
                        last_size = cur;
                        terminal = make_inline_terminal(current_inline_height)?;
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_len_strips_ansi_sequences() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len("\x1b[36mhello\x1b[0m"), 5);
        assert_eq!(visible_len("\x1b[1mRECURSIVE\x1b[0m"), 9);
    }

    #[test]
    fn visible_len_counts_cjk_as_two_columns() {
        // Two CJK characters = 4 visual columns.
        assert_eq!(visible_len("中文"), 4);
        assert_eq!(visible_len("\x1b[1m中文\x1b[0m"), 4);
    }

    #[test]
    fn pad_to_appends_spaces_to_visible_width() {
        let s = pad_to("abc", 10, "\x1b[0m");
        assert_eq!(visible_len(&s), 10);
        assert!(s.starts_with("abc"));
    }

    #[test]
    fn pad_to_is_noop_when_already_wide() {
        let s = pad_to("abcdef", 3, "\x1b[0m");
        assert_eq!(s, "abcdef");
    }

    #[test]
    fn column_widths_40_60_split() {
        // 40% of 100 = 40 left, 59 right (separator takes 1 char).
        let (l, r) = compute_column_widths(100);
        assert_eq!(l, 40);
        assert_eq!(r, 59);
    }

    #[test]
    fn column_widths_scale_with_wide_terminals() {
        // 200-char terminal: no longer hard-capped at 52.
        let (l, r) = compute_column_widths(200);
        assert_eq!(l, 80);
        assert_eq!(r, 119);
        assert!(l >= 52, "left_col should exceed the old 52-char cap");
    }

    #[test]
    fn column_widths_floor_left_at_30() {
        // Very narrow terminal: left_col floored at 30 so the 28-char
        // logo + 2 indent always fits.
        let (l, r) = compute_column_widths(60);
        assert_eq!(l, 30);
        assert_eq!(r, 29);
    }

    #[test]
    fn column_widths_sum_with_separator() {
        for w in [60, 80, 100, 120, 150, 200, 250] {
            let (l, r) = compute_column_widths(w);
            assert_eq!(l + r + 1, w, "l={l} r={r} w={w}");
        }
    }
}

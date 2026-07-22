//! Multi-mode PromptInput renderer (Goal 145).
//!
//! The input area is split into two stacked rectangles: the input
//! frame (with a one-character mode indicator on the left) and a
//! single-line footer hint (mode-dependent text on the second row).
//!
//! Real cursor positioning is computed from the prompt buffer's byte
//! offset and pushed onto the frame via
//! [`Frame::set_cursor_position`]. We deliberately don't draw a
//! synthetic glyph (`▌`) so the terminal's native cursor remains the
//! single source of truth.
//!
//! Sizing: the input pane height is `min(buffer line count + 1, 6)`
//! plus a one-row footer below.

use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, InputMode};

/// Maximum visible input rows (after which the box scrolls
/// internally). The +2 below accounts for the box's borders.
pub const MAX_VISIBLE_ROWS: u16 = 6;

/// Total height the chat layout should reserve for the input + footer
/// stack, given the current buffer.
pub fn total_height(app: &App) -> u16 {
    visible_rows(&app.prompt.buffer) + 2 /* borders */ + 1 /* footer */
}

/// Number of buffer rows we want visible.
fn visible_rows(buffer: &str) -> u16 {
    let lines = buffer.lines().count().max(1);
    // If the buffer ends in a `\n`, lines() under-counts the trailing
    // empty line; add one so the cursor on the new line is visible.
    let trailing = if buffer.ends_with('\n') { 1 } else { 0 };
    let total = lines + trailing;
    (total as u16).clamp(1, MAX_VISIBLE_ROWS)
}

/// Render the input frame + footer hint into `area`. Sets the
/// terminal cursor to the active edit position.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.height < 2 {
        return;
    }
    // Split area into [input box, hint].
    let input_h = area.height.saturating_sub(1).max(3);
    let input_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: input_h,
    };
    let hint_area = Rect {
        x: area.x,
        y: area.y + input_h,
        width: area.width,
        height: 1,
    };

    let mode = app.prompt.mode;
    let buffer = &app.prompt.buffer;
    let cursor_byte = app.prompt.cursor.min(buffer.len());

    // Build the body lines: prefix the indicator on the very first
    // visual row, plain space-padding on subsequent rows.
    let indicator_style = indicator_style(mode);
    let body_style = Style::default().fg(Color::White);

    let lines: Vec<Line<'static>> = if buffer.is_empty() {
        vec![Line::from(vec![
            Span::styled(format!("{} ", mode.indicator()), indicator_style),
            Span::styled(String::new(), body_style),
        ])]
    } else {
        buffer
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                let prefix = if i == 0 {
                    Span::styled(format!("{} ", mode.indicator()), indicator_style)
                } else {
                    Span::raw("  ")
                };
                Line::from(vec![prefix, Span::styled(line.to_string(), body_style)])
            })
            .collect()
    };

    let input = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_box_title(mode)),
    );
    frame.render_widget(input, input_area);

    // Compute cursor visual position. The input box has 1-cell border
    // and a 2-cell prefix ("X "), so the first column of editable
    // content is `area.x + 1 + 2`.
    let (col, row) = cursor_visual_position(buffer, cursor_byte);
    let cursor_x = input_area.x.saturating_add(1).saturating_add(2 + col);
    let cursor_y = input_area.y.saturating_add(1).saturating_add(row);
    // Clamp inside the input frame.
    let max_x = input_area.x + input_area.width.saturating_sub(2);
    let max_y = input_area.y + input_area.height.saturating_sub(2);
    let cx = cursor_x.min(max_x);
    let cy = cursor_y.min(max_y);
    frame.set_cursor_position(Position { x: cx, y: cy });

    // Footer hint: left = mode hint, right = live context-window usage
    // gauge. The gauge is right-aligned in the same 1-row strip so the
    // user can see how much of the model's context window is in use
    // without giving up the existing key-binding hint.
    let hint =
        Paragraph::new(footer_hint(mode)).style(Style::default().fg(Color::Gray).bg(Color::Reset));
    if let Some((gauge_text, gauge_color)) = context_gauge(app) {
        let gauge_width = unicode_width::UnicodeWidthStr::width(gauge_text.as_str()) as u16;
        // Reserve the gauge column only when there's room for both the
        // hint and the gauge; otherwise fall back to the hint alone so a
        // very narrow terminal never drops the key-binding hint.
        if hint_area.width > gauge_width.saturating_add(2) {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(gauge_width)])
                .split(hint_area);
            frame.render_widget(hint, cols[0]);
            let gauge = Paragraph::new(gauge_text)
                .style(Style::default().fg(gauge_color).bg(Color::Reset))
                .alignment(Alignment::Right);
            frame.render_widget(gauge, cols[1]);
            return;
        }
    }
    frame.render_widget(hint, hint_area);
}

/// Build the live context-window usage gauge `(text, color)` shown at the
/// bottom-right of the input box. Returns `None` when the context window
/// size is unknown (0) — e.g. before the runtime has resolved a model —
/// so we don't render a meaningless `0/0`.
///
/// `used` is [`UsageStats::current_prompt_estimate`] — the live
/// estimate that advances during tool execution (the local breakdown
/// re-estimates the conversation bucket every step), not the
/// provider-reported `last_prompt_tokens` (which only refreshes when
/// the provider returns usage). `window` is [`App::context_window`].
/// The colour ramps green → yellow → red as the window fills up so
/// the user gets an at-a-glance warning before compaction becomes
/// necessary.
fn context_gauge(app: &App) -> Option<(String, Color)> {
    let window = app.context_window;
    if window == 0 {
        return None;
    }
    let used = app.usage.current_prompt_estimate();
    let pct = (used as f64 / window as f64) * 100.0;
    let color = if pct >= 90.0 {
        Color::Red
    } else if pct >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    Some((
        format!(
            "ctx {}/{} · {:.0}%",
            human_count(used),
            human_count(window),
            pct
        ),
        color,
    ))
}

/// Compact integer formatting for the gauge: 1234 → "1.2k", 1_500_000 →
/// "1.5M". Mirrors [`crate::ui::status::human_count`] but kept local so
/// this module stays self-contained.
fn human_count(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Style of the mode indicator character on the left of the box.
fn indicator_style(mode: InputMode) -> Style {
    let fg = match mode {
        InputMode::Prompt => Color::Cyan,
        InputMode::Bash => Color::LightYellow,
        InputMode::Note => Color::DarkGray,
        InputMode::Command => Color::Magenta,
        InputMode::AtFile => Color::Cyan,
        InputMode::HistorySearch => Color::LightGreen,
        InputMode::CommandInteract => Color::Rgb(205, 100, 50),
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

/// Title shown on the input box border.
fn input_box_title(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Prompt => " Input ",
        InputMode::Bash => " Bash ",
        InputMode::Note => " Note ",
        InputMode::Command => " Command ",
        InputMode::AtFile => " @File ",
        InputMode::HistorySearch => " 🔍 History Search ",
        InputMode::CommandInteract => " Input ",
    }
}

/// Convert a buffer + byte cursor offset into a (col, row) inside the
/// edit area (zero-based, where col counts columns from the first
/// editable cell, *not* including the box border).
///
/// The input renderer pads non-first lines with two spaces in place
/// of the indicator to keep columns visually aligned, so per-line
/// content always starts at the same x. We therefore only have to
/// count `\n`s for the row, and the **display width** of the
/// preceding chars on the active line for the column. Using
/// `chars().count()` undercounts CJK / emoji / fullwidth glyphs
/// (each takes 2 columns in a terminal), which made the cursor
/// land in the middle of the previous double-width char rather
/// than after it.
pub fn cursor_visual_position(buffer: &str, cursor: usize) -> (u16, u16) {
    use unicode_width::UnicodeWidthStr;
    let head = &buffer[..cursor.min(buffer.len())];
    let row = head.matches('\n').count() as u16;
    let line_start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = UnicodeWidthStr::width(&head[line_start..]) as u16;
    (col, row)
}

/// Single-line hint shown below the input frame.
///
/// Note: the previous `ctrl+b/f or wheel scroll` segment was dropped
/// — `Ctrl+B` / `Ctrl+F` now move the cursor by one char (emacs
/// readline), and the remaining transcript-scroll affordances
/// (PageUp/PageDown, Shift+ArrowUp/Down, trackpad / mouse wheel) are
/// left implicit because the status bar already advertises the
/// mode and the scroll behaviour is terminal-native.
pub fn footer_hint(mode: InputMode) -> String {
    match mode {
        InputMode::Prompt => "⏎ submit  shift+tab mode  ↑↓ history  esc clear".into(),
        InputMode::Bash => "⏎ run shell  shift+tab mode  ↑↓ history  esc clear".into(),
        InputMode::Note => "⏎ save note  shift+tab mode  ↑↓ history  esc clear".into(),
        InputMode::Command => "⏎ run command  tab autocomplete  ↑↓ history".into(),
        InputMode::AtFile => "⏎/tab confirm  ↑↓ select  backspace edit  esc cancel".into(),
        InputMode::HistorySearch => {
            "⏎ confirm  ↑↓ select  ctrl+r next  backspace edit  esc cancel".into()
        }
        InputMode::CommandInteract => "⏎ confirm  ↑↓ select  esc cancel".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppScreen;

    #[test]
    fn renders_correct_indicator_per_mode() {
        // Build a tiny test terminal so we can inspect the rendered
        // glyphs without running the full chat layout.
        for (mode, ch) in [
            (InputMode::Prompt, '❯'),
            (InputMode::Bash, '!'),
            (InputMode::Note, '#'),
            (InputMode::Command, '/'),
        ] {
            let mut app = App::new();
            app.screen = AppScreen::Chat;
            app.prompt.mode = mode;
            let backend = ratatui::backend::TestBackend::new(40, 6);
            let mut term = ratatui::Terminal::new(backend).unwrap();
            term.draw(|f| {
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: 40,
                    height: 6,
                };
                render(f, area, &app);
            })
            .unwrap();
            let buf = term.backend().buffer();
            // Concatenate the first line into a String and assert
            // the mode glyph appears.
            let row: String = (0..buf.area().width)
                .map(|x| buf[(x, 1)].symbol())
                .collect();
            assert!(
                row.contains(ch),
                "mode {mode:?} indicator {ch:?} missing from row {row:?}"
            );
        }
    }

    #[test]
    fn footer_hint_changes_per_mode() {
        assert!(footer_hint(InputMode::Prompt).contains("submit"));
        assert!(footer_hint(InputMode::Bash).contains("run shell"));
        assert!(footer_hint(InputMode::Note).contains("save note"));
        assert!(footer_hint(InputMode::Command).contains("run command"));
    }

    #[test]
    fn cursor_visual_position_handles_multiline() {
        let buf = "ab\ncde\nf";
        // Cursor at very end (byte 8): row=2, col=1 ("f")
        assert_eq!(cursor_visual_position(buf, buf.len()), (1, 2));
        // Cursor at start of line 2 (just after first '\n', byte 3)
        assert_eq!(cursor_visual_position(buf, 3), (0, 1));
        // Cursor at byte 0
        assert_eq!(cursor_visual_position(buf, 0), (0, 0));
    }

    /// Goal-150 follow-up: CJK / fullwidth chars take two terminal
    /// columns each; `chars().count()` (the previous implementation)
    /// undercounted them and left the cursor visually inside the
    /// preceding glyph rather than after it.
    #[test]
    fn cursor_visual_position_counts_double_width_chars() {
        let buf = "你好";
        // Two Chinese chars = 6 bytes (3 each), 4 visual columns.
        assert_eq!(buf.len(), 6);
        // Cursor after the first char (byte 3): col=2 (one CJK glyph).
        assert_eq!(cursor_visual_position(buf, 3), (2, 0));
        // Cursor at the end (byte 6): col=4 (two CJK glyphs).
        assert_eq!(cursor_visual_position(buf, buf.len()), (4, 0));
    }

    #[test]
    fn cursor_visual_position_mixed_ascii_and_cjk() {
        // "ab了" — 'a' + 'b' (1 col each) + '了' (2 cols) = 4 cols.
        let buf = "ab了";
        assert_eq!(cursor_visual_position(buf, buf.len()), (4, 0));
        // After "ab" (byte 2): col=2.
        assert_eq!(cursor_visual_position(buf, 2), (2, 0));
    }

    #[test]
    fn total_height_grows_with_lines_until_cap() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.buffer = "a".into();
        let h1 = total_height(&app);
        app.prompt.buffer = "a\nb\nc\nd\ne\nf\ng".into();
        let h_max = total_height(&app);
        assert!(h_max > h1);
        // The input box itself is capped at MAX_VISIBLE_ROWS rows of
        // editable area, plus 2 for borders, plus 1 for the footer.
        assert!(h_max <= MAX_VISIBLE_ROWS + 2 + 1);
    }

    #[test]
    fn render_draws_input_box_when_height_is_two() {
        // area.height == 2: orig `2 < 2` is false -> renders the input
        // box (top border visible). mutant `<=` (45:20): `2 <= 2` is true
        // -> early return -> blank buffer. Use a taller frame so the
        // hint_area (y = 3) stays in-bounds and orig doesn't panic.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let app = App::new();
        let backend = TestBackend::new(40, 5);
        let mut term = Terminal::new(backend).expect("TestBackend infallible");
        term.draw(|fr| render(fr, Rect::new(0, 0, 40, 2), &app))
            .expect("draw infallible");
        let buf = term.backend().buffer();
        let row: String = (0..40)
            .map(|x| {
                buf.cell((x, 0))
                    .expect("cell")
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' ')
            })
            .collect();
        assert!(
            !row.trim().is_empty(),
            "expected the input box to be rendered at height 2; got {row:?}"
        );
    }

    #[test]
    fn visible_rows_counts_trailing_newline_as_extra_row() {
        // "a\n" -> lines() yields 1, trailing +1 -> total 2. mutant
        // `+`->`-` (38:23): 1 - 1 = 0 -> clamp(1) = 1.
        assert_eq!(visible_rows("a\n"), 2);
        assert_eq!(visible_rows("a"), 1);
    }

    #[test]
    fn indicator_style_has_mode_colour_and_bold() {
        // kills indicator_style -> Default::default() (120:5).
        let style = indicator_style(InputMode::Prompt);
        assert_eq!(style.fg, Some(Color::Cyan));
        assert!(
            (style.add_modifier & Modifier::BOLD) == Modifier::BOLD,
            "indicator style should be bold"
        );
    }

    #[test]
    fn input_box_title_returns_mode_label() {
        // kills input_box_title -> ""/"xyzzy" (134:5).
        assert_eq!(input_box_title(InputMode::Prompt), " Input ");
        assert_eq!(input_box_title(InputMode::Bash), " Bash ");
    }

    #[test]
    fn render_first_line_prefix_uses_indicator_on_first_row() {
        // Multi-line buffer: orig prefixes the first row with the mode
        // indicator (`! ` for Bash) and subsequent rows with two spaces.
        // mutant `==`->`!=` (82:35) swaps them -> the `a` row gets `  `
        // and the `b` row gets `! `, so `! a` never appears.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new();
        app.prompt.mode = InputMode::Bash;
        app.prompt.buffer = "a\nb".into();
        let backend = TestBackend::new(40, 8);
        let mut term = Terminal::new(backend).expect("TestBackend infallible");
        term.draw(|fr| render(fr, fr.area(), &app))
            .expect("draw infallible");
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf.cell((x, y)).expect("cell").symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("! a"),
            "expected `! a` prefix on the first content row; got {text:?}"
        );
    }

    // ── context-window usage gauge (footer-right) ─────────────────────────

    #[test]
    fn context_gauge_returns_none_when_window_unknown() {
        // context_window == 0 (e.g. before runtime resolved a model) →
        // no gauge, so we never render a misleading `0/0`.
        let mut app = App::new();
        app.context_window = 0;
        app.usage.last_prompt_tokens = 1234;
        assert!(context_gauge(&app).is_none());
    }

    #[test]
    fn context_gauge_formats_used_over_window_with_pct() {
        let mut app = App::new();
        app.context_window = 128_000;
        app.usage.last_prompt_tokens = 12_345;
        let (text, _color) = context_gauge(&app).expect("gauge should be present");
        assert!(
            text.contains("12.3k"),
            "expected compact used tokens, got: {text}"
        );
        assert!(
            text.contains("128.0k"),
            "expected compact window tokens, got: {text}"
        );
        // 12345 / 128000 ≈ 9.6% → rounds to 10%.
        assert!(text.contains("10%"), "expected ~10% usage, got: {text}");
    }

    #[test]
    fn context_gauge_color_ramps_with_usage() {
        let mut app = App::new();
        app.context_window = 100_000;
        // < 70% → green.
        app.usage.last_prompt_tokens = 10_000;
        assert_eq!(context_gauge(&app).unwrap().1, Color::Green);
        // 70%–90% → yellow.
        app.usage.last_prompt_tokens = 75_000;
        assert_eq!(context_gauge(&app).unwrap().1, Color::Yellow);
        // >= 90% → red.
        app.usage.last_prompt_tokens = 95_000;
        assert_eq!(context_gauge(&app).unwrap().1, Color::Red);
    }

    #[test]
    fn render_draws_gauge_in_footer_when_room_available() {
        // Wide-enough terminal: the gauge text must appear on the footer
        // (hint) row, right-aligned. Pins the split-and-render path so a
        // mutant that skips the gauge branch is caught.
        let mut app = App::new();
        app.context_window = 128_000;
        app.usage.last_prompt_tokens = 12_345;
        let backend = ratatui::backend::TestBackend::new(80, 6);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &app)).unwrap();
        let buf = term.backend().buffer();
        // The footer hint is the last row of the rendered area.
        let footer_y = buf.area().height - 1;
        let footer: String = (0..buf.area().width)
            .map(|x| buf[(x, footer_y)].symbol())
            .collect();
        assert!(
            footer.contains("ctx"),
            "expected gauge on footer row, got: {footer:?}"
        );
        assert!(
            footer.contains("10%"),
            "expected usage pct on footer row, got: {footer:?}"
        );
    }

    #[test]
    fn render_falls_back_to_hint_only_on_narrow_terminal() {
        // Very narrow terminal: not enough room for both hint and gauge,
        // so the hint must still render and the gauge must be dropped
        // rather than overflowing / clobbering the hint.
        let mut app = App::new();
        app.context_window = 128_000;
        app.usage.last_prompt_tokens = 12_345;
        let backend = ratatui::backend::TestBackend::new(10, 6);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &app)).unwrap();
        let buf = term.backend().buffer();
        let footer_y = buf.area().height - 1;
        let footer: String = (0..buf.area().width)
            .map(|x| buf[(x, footer_y)].symbol())
            .collect();
        // The mode hint ("submit" for Prompt) should still be present.
        assert!(
            footer.contains("submit"),
            "hint should remain on narrow terminal, got: {footer:?}"
        );
    }
}

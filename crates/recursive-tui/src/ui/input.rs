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
use unicode_width::UnicodeWidthStr;

use crate::app::{App, InputMode};

/// Maximum visible input rows (after which the box scrolls
/// internally). The +2 below accounts for the box's borders.
pub const MAX_VISIBLE_ROWS: u16 = 6;

/// Columns reserved on the left/right of the input box: the `Block`
/// borders (1 + 1).
const BORDER_WIDTH: u16 = 2;
/// Columns reserved on the first content row: the mode indicator plus a
/// separating space (e.g. `❯ `).
const PREFIX_WIDTH: u16 = 2;

/// Total height the chat layout should reserve for the input + footer
/// stack, given the current buffer.
///
/// `area_width` is the width of the rectangle the chat layout will hand
/// to [`render`] — the input box spans the full terminal width. We fold
/// every logical line with [`wrap_line_by_width`] so a pasted long
/// paragraph (no `'\n'`) expands the box just like many short lines
/// would; counting only logical lines (the old estimator) left a long
/// paste stuck at one row and clipped it on render.
pub fn total_height(app: &App, area_width: u16) -> u16 {
    let avail = available_text_width_from(area_width);
    let buf = &app.prompt.buffer;

    // `split('\n')` already turns a trailing newline into a trailing
    // empty segment, so we don't need the old `ends_with('\n')` +1
    // correction — the segment count is the visual row count.
    let mut visual_lines: u16 = 0;
    for line in buf.split('\n') {
        visual_lines = visual_lines.saturating_add(wrap_line_by_width(line, avail).len() as u16);
    }
    // An empty buffer still yields one segment, so this `.max(1)` is a
    // belt-and-braces guard for the saturating-add path above.
    let visual_lines = visual_lines.max(1);

    visual_lines
        .clamp(1, MAX_VISIBLE_ROWS)
        .saturating_add(BORDER_WIDTH)
        .saturating_add(1 /* footer */)
}

/// Calculate the available width for text content in the input box from
/// a rendered [`Rect`] (used by [`render`]).
fn available_text_width(area: Rect) -> usize {
    available_text_width_from(area.width)
}

/// Same width math as [`available_text_width`] but starting from a bare
/// column count, so [`total_height`] can estimate before any `Rect`
/// exists — the chat layout calls it to size the very `Rect` it will
/// later pass to [`render`].
fn available_text_width_from(area_width: u16) -> usize {
    area_width.saturating_sub(BORDER_WIDTH.saturating_add(PREFIX_WIDTH)) as usize
}

/// Wrap a single line of text to fit within the available width.
/// Returns a vector of line segments that fit within the width constraint.
fn wrap_line_by_width(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || line.is_empty() {
        return vec![line.to_string()];
    }

    let mut wrapped_lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for ch in line.chars() {
        // Calculate display width of this character using a slice
        let mut char_buf = [0u8; 4];
        let ch_str = ch.encode_utf8(&mut char_buf);
        let ch_width = UnicodeWidthStr::width(ch_str);

        if current_width + ch_width > max_width && !current_line.is_empty() {
            // Start a new line
            wrapped_lines.push(current_line);
            current_line = String::new();
            current_width = 0;
        }

        current_line.push(ch);
        current_width += ch_width;
    }

    if !current_line.is_empty() {
        wrapped_lines.push(current_line);
    }

    if wrapped_lines.is_empty() {
        wrapped_lines.push(String::new());
    }

    wrapped_lines
}

/// Calculate the visual position of the cursor in the wrapped display.
/// Returns (col, row) where col is the visual column within the line
/// and row is the visual row number in the displayed text.
fn cursor_visual_position_wrapped(buffer: &str, cursor: usize, avail_width: usize) -> (u16, u16) {
    let head = &buffer[..cursor.min(buffer.len())];

    // Count how many logical newlines are before the cursor
    let logical_newlines_before = head.matches('\n').count();

    // Find the start of the current logical line
    let line_start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let current_logical_line = &buffer[line_start..cursor.min(buffer.len())];

    // Calculate how many wrapped lines come from previous logical lines
    let mut wrapped_lines_before = 0u16;

    // Process all complete logical lines before the current one
    for (i, line) in buffer.split('\n').enumerate() {
        if i < logical_newlines_before {
            let wrapped = wrap_line_by_width(line, avail_width);
            wrapped_lines_before += wrapped.len() as u16;
        } else if i == logical_newlines_before {
            // This is the current logical line - calculate position within wrapped lines
            let wrapped = wrap_line_by_width(line, avail_width);

            // Find where the cursor falls within the wrapped lines
            let mut byte_offset = 0;
            let mut target_row = 0u16;
            let mut target_col = 0u16;

            for (row_idx, wrapped_line) in wrapped.iter().enumerate() {
                let line_bytes = wrapped_line.len();
                if byte_offset + line_bytes >= current_logical_line.len() {
                    // Cursor is on this wrapped line
                    target_col =
                        UnicodeWidthStr::width(&current_logical_line[byte_offset..]) as u16;
                    target_row = row_idx as u16;
                    break;
                } else {
                    byte_offset += line_bytes;
                }
            }

            return (target_col, wrapped_lines_before + target_row);
        }
    }

    // Fallback for empty buffer
    (0, 0)
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

    // Calculate available width for text content
    let avail_width = available_text_width(input_area);

    // Build the body lines: prefix the indicator on the very first
    // visual row, plain space-padding on subsequent rows.
    // Now handles long lines by wrapping them to fit the available width.
    let indicator_style = indicator_style(mode);
    let body_style = Style::default().fg(Color::White);

    let lines: Vec<Line<'static>> = if buffer.is_empty() {
        vec![Line::from(vec![
            Span::styled(format!("{} ", mode.indicator()), indicator_style),
            Span::styled(String::new(), body_style),
        ])]
    } else {
        let mut all_lines = Vec::new();
        let raw_lines: Vec<&str> = buffer.split('\n').collect();

        for (i, line) in raw_lines.iter().enumerate() {
            let is_first_logical_line = i == 0;

            // Wrap long lines to fit within available width
            let wrapped = wrap_line_by_width(line, avail_width);

            for (j, wrapped_line) in wrapped.iter().enumerate() {
                let is_first_visual_row = is_first_logical_line && j == 0;
                let prefix = if is_first_visual_row {
                    Span::styled(format!("{} ", mode.indicator()), indicator_style)
                } else {
                    Span::raw("  ")
                };
                all_lines.push(Line::from(vec![
                    prefix.clone(),
                    Span::styled(wrapped_line.to_string(), body_style),
                ]));
            }
        }

        all_lines
    };

    // Compute cursor visual position with wrapped lines considered,
    // BEFORE rendering, so we can derive the vertical scroll offset that
    // keeps the cursor's wrapped row inside the visible content window.
    // The input box has 1-cell border and a 2-cell prefix ("X "),
    // so the first column of editable content is `area.x + 1 + 2`.
    let (col, cursor_row) = cursor_visual_position_wrapped(buffer, cursor_byte, avail_width);

    // Visible content rows inside the box (height minus the two border
    // rows). Capped at MAX_VISIBLE_ROWS by the layout, but compute from
    // the actual area so a squeezed terminal still behaves.
    let visible_rows = input_area.height.saturating_sub(BORDER_WIDTH).max(1);
    // Scroll so the cursor row stays within the window. We follow the
    // cursor the way an editor does: only scroll when the cursor would
    // fall outside `[scroll_y, scroll_y + visible_rows)`. This is what
    // lets the user paste a long paragraph and then keep typing at the
    // end — without it, `Paragraph` is top-anchored and clips every row
    // past `visible_rows`, hiding the cursor's real edit position.
    let scroll_y = cursor_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(cursor_row);

    let input = Paragraph::new(lines).scroll((scroll_y, 0)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_box_title(mode)),
    );
    frame.render_widget(input, input_area);

    // Translate the cursor's wrapped row back to screen coordinates by
    // subtracting the scroll offset, then clamp inside the input frame.
    let cursor_x = input_area
        .x
        .saturating_add(1)
        .saturating_add(PREFIX_WIDTH + col);
    let cursor_y = input_area
        .y
        .saturating_add(1)
        .saturating_add(cursor_row.saturating_sub(scroll_y));
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
    fn cursor_visual_position_wrapped_empty_buffer() {
        // Empty buffer → origin, regardless of width.
        assert_eq!(cursor_visual_position_wrapped("", 0, 10), (0, 0));
        assert_eq!(cursor_visual_position_wrapped("", 0, 0), (0, 0));
    }

    #[test]
    fn cursor_visual_position_wrapped_short_line_does_not_wrap() {
        // "abc" fits in 10 cols → single row, column tracks byte cursor.
        let buf = "abc";
        assert_eq!(cursor_visual_position_wrapped(buf, 0, 10), (0, 0));
        assert_eq!(cursor_visual_position_wrapped(buf, 1, 10), (1, 0));
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 10), (3, 0));
    }

    /// "hello world" (11 bytes) at width 5 wraps to ["hello", " worl", "d"].
    /// Pins the row detection (byte accumulation across wrapped rows) and
    /// the column (display width of the prefix on the matched row).
    #[test]
    fn cursor_visual_position_wrapped_long_line_rows_and_cols() {
        let buf = "hello world";
        // End of buffer → last wrapped row "d", col 1.
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 5), (1, 2));
        // Right after "hello" (byte 5) — still on row 0 at col 5.
        assert_eq!(cursor_visual_position_wrapped(buf, 5, 5), (5, 0));
        // After the space (byte 6) — wrapped onto row 1, col 1.
        assert_eq!(cursor_visual_position_wrapped(buf, 6, 5), (1, 1));
    }

    /// A wrapped logical line BEFORE the cursor's line must contribute its
    /// wrapped-row count to the cursor's absolute row.
    #[test]
    fn cursor_visual_position_wrapped_multiline_counts_preceding_wraps() {
        // "aaaaaa" wraps to ["aaaaa", "a"] (2 rows) at width 5; "bb" is line 1.
        let buf = "aaaaaa\nbb";
        // End of buffer → line 1 "bb", col 2, absolute row 2.
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 5), (2, 2));
        // Start of line 1 (byte 7, right after '\n') → col 0, row 2.
        assert_eq!(cursor_visual_position_wrapped(buf, 7, 5), (0, 2));
    }

    /// Two preceding logical lines that EACH wrap must SUM their wrapped-row
    /// counts. A single preceding line can't tell `+=` from `=`, so this is
    /// the case that catches the `wrapped_lines_before = wrapped.len()`
    /// mutant (assign instead of compound-add).
    #[test]
    fn cursor_visual_position_wrapped_sums_multiple_preceding_wraps() {
        // "aaaaaa" → ["aaaaa","a"] (2); "bbbbbb" → ["bbbbb","b"] (2); "c" on
        // line 2. At width 5 the cursor's absolute row is 2 + 2 + 0 = 4.
        let buf = "aaaaaa\nbbbbbb\nc";
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 5), (1, 4));
    }

    /// CJK glyphs are 3 bytes but 2 display columns. The row-finder works in
    /// bytes (wrapped-line lengths) while the column is display width, so a
    /// pure-ASCII assumption would miscount either side.
    #[test]
    fn cursor_visual_position_wrapped_double_width_chars() {
        // "你好你好" = 12 bytes, 8 cols. At width 4 → ["你好", "你好"].
        let buf = "你好你好";
        assert_eq!(buf.len(), 12);
        // End → row 1, col 4 (width of "你好").
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 4), (4, 1));
        // After first "你好" (byte 6) → row 0, col 4.
        assert_eq!(cursor_visual_position_wrapped(buf, 6, 4), (4, 0));
    }

    /// Mixed ASCII + CJK inside one wrapped row: "ab了" is 5 bytes / 4 cols,
    /// so the wrap boundary and the column come out differently. This is the
    /// case that breaks any implementation confusing byte offset with width.
    #[test]
    fn cursor_visual_position_wrapped_mixed_ascii_and_cjk() {
        // "ab了cd" = 7 bytes / 6 cols. At width 4 → ["ab了", "cd"].
        let buf = "ab了cd";
        // End (byte 7) → row 1 "cd", col 2.
        assert_eq!(cursor_visual_position_wrapped(buf, buf.len(), 4), (2, 1));
        // After "ab了" (byte 5) → row 0, col 4 (display width, not byte count).
        assert_eq!(cursor_visual_position_wrapped(buf, 5, 4), (4, 0));
    }

    #[test]
    fn total_height_grows_with_lines_until_cap() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;
        app.prompt.buffer = "a".into();
        let h1 = total_height(&app, 80);
        app.prompt.buffer = "a\nb\nc\nd\ne\nf\ng".into();
        let h_max = total_height(&app, 80);
        assert!(h_max > h1);
        // The input box itself is capped at MAX_VISIBLE_ROWS rows of
        // editable area, plus 2 for borders, plus 1 for the footer.
        assert!(h_max <= MAX_VISIBLE_ROWS + 2 + 1);
    }

    /// A long single-line paste (no `'\n'`) MUST expand the box the same
    /// way many short lines do. This is the regression for the original
    /// bug: `total_height` used to count only logical lines, so a long
    /// paste was estimated at one row and got clipped on render.
    #[test]
    fn total_height_grows_with_wrapped_long_line() {
        let mut app = App::new();
        app.screen = AppScreen::Chat;

        // Wide terminal → short buffer fits on one visual row.
        app.prompt.buffer = "a".into();
        let h_short = total_height(&app, 80);

        // Same logical-line count (1) but content that wraps to many
        // visual rows at width 10. avail = 10 - 4 = 6 cols; "abcdef" is
        // one row, "abcdefghijkl" → 2 rows, "abcdefghijklmn" → 3 rows.
        app.prompt.buffer = "abcdefghijklmn".into(); // 14 chars → 3 rows @ 6 cols
        let h_long = total_height(&app, 10);

        // Same number of logical lines, but the wrapped estimate must be
        // taller than the single-row estimate.
        assert!(
            h_long > h_short,
            "a long wrapped line should grow the box past one row: h_long={h_long} h_short={h_short}"
        );
        // 3 visual rows + 2 border + 1 footer.
        assert_eq!(h_long, 3 + 2 + 1);

        // Push well past MAX_VISIBLE_ROWS: height must cap, not overflow.
        app.prompt.buffer = "a".repeat(200);
        let h_cap = total_height(&app, 10);
        assert_eq!(h_cap, MAX_VISIBLE_ROWS + 2 + 1);
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
    fn wrap_line_by_width_handles_long_text() {
        // Test basic wrapping
        let result = wrap_line_by_width("hello world", 5);
        assert_eq!(result, vec!["hello", " worl", "d"]);

        // Test empty string
        let result = wrap_line_by_width("", 10);
        assert_eq!(result, vec![""]);

        // Test zero width (should not wrap)
        let result = wrap_line_by_width("hello", 0);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn wrap_line_handles_wide_characters() {
        // Test CJK characters that take 2 columns
        let result = wrap_line_by_width("你好世界", 4);
        // Each CJK char is 2 columns, so "你好" = 4 cols fits on one line
        // and "世界" = 4 cols on the next line
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "你好");
        assert_eq!(result[1], "世界");
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

    /// When the buffer wraps to more rows than the box can show AND the
    /// cursor is at the end (the paste-then-keep-typing case), the
    /// renderer must scroll so the cursor's row is visible. Without the
    /// scroll, `Paragraph` is top-anchored and the tail of a long paste
    /// is clipped off-screen — the original bug.
    #[test]
    fn render_scrolls_to_keep_cursor_visible_when_buffer_overflows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new();
        app.screen = AppScreen::Chat;

        // Build a long single-line buffer that wraps to MANY rows at the
        // narrow width we render with. End it with a unique sentinel so
        // we can assert its presence on screen. 30 'A's + "Z" = 31 chars.
        // At avail width 4 → 8 wrapped rows (4 chars per row). The box
        // (area height 6 → 4 content rows after borders) can't show all
        // 8 at once, so the tail "Z" is only visible if we scroll to it.
        app.prompt.buffer = format!("{}Z", "A".repeat(30));
        app.prompt.cursor = app.prompt.buffer.len(); // cursor at the end

        // area width 8 → avail = 8 - 4 = 4 cols. area height 6 → input
        // box height 5 (6 - 1 footer) → visible content rows = 3.
        let backend = TestBackend::new(8, 6);
        let mut term = Terminal::new(backend).expect("TestBackend infallible");
        term.draw(|fr| render(fr, fr.area(), &app))
            .expect("draw infallible");
        let buf = term.backend().buffer();

        // The whole screen — the sentinel must be somewhere visible.
        let mut all = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                all.push_str(buf.cell((x, y)).expect("cell").symbol());
            }
            all.push('\n');
        }
        assert!(
            all.contains('Z'),
            "cursor's tail row should be scrolled into view; got screen:\n{all}"
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

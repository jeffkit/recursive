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

use crate::tui::app::{App, InputMode};

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

    // Footer hint.
    let hint =
        Paragraph::new(footer_hint(mode)).style(Style::default().fg(Color::Gray).bg(Color::Reset));
    frame.render_widget(hint, hint_area);
}

/// Style of the mode indicator character on the left of the box.
fn indicator_style(mode: InputMode) -> Style {
    let fg = match mode {
        InputMode::Prompt => Color::Cyan,
        InputMode::Bash => Color::LightYellow,
        InputMode::Note => Color::DarkGray,
        InputMode::Command => Color::Magenta,
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
pub fn footer_hint(mode: InputMode) -> String {
    match mode {
        InputMode::Prompt => {
            "⏎ submit  shift+tab mode  ↑↓ history  ctrl+b/f or wheel scroll  esc clear".into()
        }
        InputMode::Bash => {
            "⏎ run shell  shift+tab mode  ↑↓ history  ctrl+b/f or wheel scroll  esc clear".into()
        }
        InputMode::Note => {
            "⏎ save note  shift+tab mode  ↑↓ history  ctrl+b/f or wheel scroll  esc clear".into()
        }
        InputMode::Command => {
            "⏎ run command  tab autocomplete  ↑↓ history  ctrl+b/f or wheel scroll".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::AppScreen;

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
}

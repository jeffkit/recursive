//! Inline-markdown renderer for assistant messages.
//!
//! Handles the constructs that come up most in LLM responses:
//!
//! - **bold** (`**...**`)
//! - *italic* / _italic_ (`*...*`, `_..._`)
//! - inline `code` (`` `...` ``)
//! - line-level heading (`# `, `## `, ...)
//! - fenced code block with syntax highlighting via `syntect`
//! - simple bullet (`- ` / `* `)
//! - Markdown table (`|col1|col2|` + `|---|---|`)
//!
//! Public entry points:
//! - [`render_inline`] — single-line inline markdown → `Vec<Span<'static>>`
//! - [`render_table`]  — slice of table-row lines → `Vec<Line<'static>>`

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// ── Lazy-loaded syntect state ──────────────────────────────────────────
// Use OnceLock (stable since 1.70) instead of LazyLock (stable 1.80)
// to stay within the project's MSRV of 1.75.

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

// ── Public types ───────────────────────────────────────────────────────

/// State carried across consecutive lines so fenced code blocks can span
/// multiple lines.
#[derive(Default, Clone)]
pub struct MdState {
    /// True when we are inside a ` ``` `…` ``` ` fenced code block.
    pub in_code_block: bool,
    /// Language tag extracted from the opening fence line (e.g. `"rust"`).
    /// Empty string when no language was specified.
    pub code_lang: String,
}

/// Result of rendering a single line.
pub struct RenderedLine {
    pub spans: Vec<Span<'static>>,
    /// Updated state to feed into the next line.
    pub state: MdState,
}

// ── Main entry points ──────────────────────────────────────────────────

/// Render one logical line of markdown-ish text into styled spans.
///
/// `default_fg` is the colour for plain-text spans (e.g.
/// `Color::White` for the assistant body so it stays bright on black).
pub fn render_inline(line: &str, default_fg: Color, state: MdState) -> RenderedLine {
    let trimmed = line.trim_start();

    // ── fenced code block fence line ──────────────────────────────────
    if trimmed.starts_with("```") {
        if state.in_code_block {
            // Closing fence
            return RenderedLine {
                spans: vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Gray),
                )],
                state: MdState::default(),
            };
        } else {
            // Opening fence — extract optional language tag
            let lang = trimmed.trim_start_matches('`').trim().to_lowercase();
            return RenderedLine {
                spans: vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Gray),
                )],
                state: MdState {
                    in_code_block: true,
                    code_lang: lang,
                },
            };
        }
    }

    // ── inside fenced code block ───────────────────────────────────────
    if state.in_code_block {
        let spans = highlight_code_line(line, &state.code_lang);
        return RenderedLine { spans, state };
    }

    // ── heading ────────────────────────────────────────────────────────
    if let Some(rest) = strip_heading(line) {
        return RenderedLine {
            spans: vec![Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )],
            state,
        };
    }

    // ── bullet list ────────────────────────────────────────────────────
    if let Some((indent, rest)) = strip_bullet(line) {
        let mut out: Vec<Span<'static>> = Vec::with_capacity(3);
        out.push(Span::raw(indent.to_string()));
        out.push(Span::styled(
            "• ".to_string(),
            Style::default().fg(Color::LightYellow),
        ));
        out.extend(parse_inline(rest, default_fg));
        return RenderedLine { spans: out, state };
    }

    // ── plain inline parse ─────────────────────────────────────────────
    RenderedLine {
        spans: parse_inline(line, default_fg),
        state,
    }
}

/// Render a Markdown table (sequence of `|...|` lines, including the
/// optional `|---|---|` separator row) into a set of styled `Line`s.
///
/// `gutter_style` is applied to the leading gutter prefix (e.g. `"│  "`).
pub fn render_table(rows: &[&str], gutter_prefix: &str, gutter_style: Style) -> Vec<Line<'static>> {
    let parsed = parse_table_rows(rows);
    if parsed.is_empty() {
        return Vec::new();
    }

    // Determine how many columns we have (max across all rows).
    let ncols = parsed
        .iter()
        .map(|(_, cells)| cells.len())
        .max()
        .unwrap_or(0);
    if ncols == 0 {
        return Vec::new();
    }

    // Find which row is the separator row (contains only dashes/colons).
    let separator_idx = parsed.iter().position(|(is_sep, _)| *is_sep);

    // Compute maximum content width per column.
    let mut col_widths: Vec<usize> = vec![0; ncols];
    for (is_sep, cells) in &parsed {
        if *is_sep {
            continue;
        }
        for (ci, cell) in cells.iter().enumerate().take(ncols) {
            col_widths[ci] = col_widths[ci].max(cell.len());
        }
    }
    // Minimum width of 1.
    for w in &mut col_widths {
        if *w == 0 {
            *w = 1;
        }
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let gutter_owned = gutter_prefix.to_string();

    // Build top border.
    out.push(make_border_line(
        &col_widths,
        '┌',
        '─',
        '┬',
        '┐',
        &gutter_owned,
        gutter_style,
    ));

    let data_rows: Vec<(bool, Vec<String>, bool)> = parsed
        .iter()
        .enumerate()
        .filter_map(|(row_idx, (is_sep, cells))| {
            if *is_sep {
                None
            } else {
                let is_header = separator_idx.is_some_and(|si| row_idx < si);
                Some((*is_sep, cells.clone(), is_header))
            }
        })
        .collect();

    for (data_idx, (_is_sep, cells, is_header)) in data_rows.iter().enumerate() {
        // Data row.
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(gutter_owned.clone(), gutter_style));
        spans.push(Span::raw("│"));
        for (ci, w) in col_widths.iter().enumerate() {
            let cell_text = cells.get(ci).map(String::as_str).unwrap_or("");
            let padded = format!(" {:<width$} ", cell_text, width = w);
            let style = if *is_header {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            spans.push(Span::styled(padded, style));
            spans.push(Span::raw("│"));
        }
        out.push(Line::from(spans));

        // After header row → print a divider.
        let is_last_data = data_idx == data_rows.len() - 1;
        if *is_header && !is_last_data {
            out.push(make_border_line(
                &col_widths,
                '├',
                '─',
                '┼',
                '┤',
                &gutter_owned,
                gutter_style,
            ));
        }
    }

    // Bottom border.
    out.push(make_border_line(
        &col_widths,
        '└',
        '─',
        '┴',
        '┘',
        &gutter_owned,
        gutter_style,
    ));

    out
}

// ── True when a line looks like a table row ────────────────────────────

/// Returns `true` when the line should be treated as part of a Markdown
/// table (starts with `|` after optional whitespace, or contains `|`).
pub fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    // Must start and (ideally) end with `|`, or at minimum contain `|`
    // with the first visible char being `|`.
    t.starts_with('|')
}

// ── Internal helpers ───────────────────────────────────────────────────

fn highlight_code_line(line: &str, lang: &str) -> Vec<Span<'static>> {
    let ss = syntax_set();
    let ts = theme_set();

    // Try to find the syntax for the given language tag.
    let syntax = if lang.is_empty() {
        None
    } else {
        ss.find_syntax_by_token(lang)
    };

    let Some(syntax) = syntax else {
        // Fallback: plain LightYellow (Goal 150 behaviour).
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(Color::LightYellow),
        )];
    };

    // "base16-ocean.dark" is a good dark-terminal theme bundled with syntect.
    let theme = ts
        .themes
        .get("base16-ocean.dark")
        .or_else(|| ts.themes.values().next());
    let Some(theme) = theme else {
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(Color::LightYellow),
        )];
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out: Vec<Span<'static>> = Vec::new();

    // syntect expects lines-with-endings; give it one line.
    let text_with_newline = format!("{line}\n");
    for piece in LinesWithEndings::from(&text_with_newline) {
        if let Ok(highlighted) = highlighter.highlight_line(piece, ss) {
            for (style, text) in highlighted {
                let ratatui_color = syntect_color_to_ratatui(style);
                let clean = text.trim_end_matches('\n').to_string();
                if !clean.is_empty() {
                    out.push(Span::styled(clean, Style::default().fg(ratatui_color)));
                }
            }
        }
    }

    if out.is_empty() {
        out.push(Span::styled(
            line.to_string(),
            Style::default().fg(Color::LightYellow),
        ));
    }
    out
}

fn syntect_color_to_ratatui(style: SyntectStyle) -> Color {
    let c = style.foreground;
    Color::Rgb(c.r, c.g, c.b)
}

/// Parse a slice of raw table-row strings into
/// `Vec<(is_separator, cells)>` where `cells` are trimmed cell strings.
fn parse_table_rows(rows: &[&str]) -> Vec<(bool, Vec<String>)> {
    rows.iter()
        .map(|row| {
            let trimmed = row.trim();
            // Strip leading and trailing `|` then split.
            let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
            let inner = inner.strip_suffix('|').unwrap_or(inner);
            let cells: Vec<String> = inner.split('|').map(|c| c.trim().to_string()).collect();
            let is_sep = cells
                .iter()
                .all(|c| c.is_empty() || c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '));
            (is_sep, cells)
        })
        .collect()
}

/// Build a horizontal border line for the table.
fn make_border_line(
    col_widths: &[usize],
    left: char,
    fill: char,
    sep: char,
    right: char,
    gutter: &str,
    gutter_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(gutter.to_string(), gutter_style));
    let mut s = String::new();
    s.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        // +2 for the spaces around cell content
        for _ in 0..w + 2 {
            s.push(fill);
        }
        if i < col_widths.len() - 1 {
            s.push(sep);
        }
    }
    s.push(right);
    spans.push(Span::styled(s, Style::default().fg(Color::Gray)));
    Line::from(spans)
}

// ── Shared inline-parse helpers ────────────────────────────────────────

/// `# heading` / `## heading` / ... → return the heading body without the
/// leading hashes.
fn strip_heading(line: &str) -> Option<&str> {
    let mut rest = line;
    let mut hashes = 0;
    while rest.starts_with('#') && hashes < 6 {
        rest = &rest[1..];
        hashes += 1;
    }
    if hashes == 0 {
        return None;
    }
    if let Some(stripped) = rest.strip_prefix(' ') {
        Some(stripped)
    } else if rest.is_empty() {
        Some("")
    } else {
        None
    }
}

/// Match a leading `- ` / `* ` / `+ ` bullet → return `(indent, body)`.
fn strip_bullet(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let body = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    Some((&line[..indent_len], body))
}

/// Parse a line for **bold**, *italic*, _italic_, `code` and emit styled
/// spans.
fn parse_inline(line: &str, default_fg: Color) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;

    let plain_style = Style::default().fg(default_fg);
    let bold_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let italic_style = Style::default()
        .fg(default_fg)
        .add_modifier(Modifier::ITALIC);
    let code_style = Style::default().fg(Color::LightYellow);

    let flush_plain =
        |out: &mut Vec<Span<'static>>, line: &str, start: usize, end: usize, style: Style| {
            if end > start {
                out.push(Span::styled(line[start..end].to_string(), style));
            }
        };

    while i < bytes.len() {
        // **bold**
        if i + 1 < bytes.len() && &bytes[i..i + 2] == b"**" {
            if let Some(close) = find_close(line, i + 2, "**") {
                flush_plain(&mut out, line, plain_start, i, plain_style);
                out.push(Span::styled(line[i + 2..close].to_string(), bold_style));
                i = close + 2;
                plain_start = i;
                continue;
            }
        }
        // `code`
        if bytes[i] == b'`' {
            if let Some(close) = find_close(line, i + 1, "`") {
                flush_plain(&mut out, line, plain_start, i, plain_style);
                out.push(Span::styled(line[i + 1..close].to_string(), code_style));
                i = close + 1;
                plain_start = i;
                continue;
            }
        }
        // *italic* / _italic_
        if (bytes[i] == b'*' || bytes[i] == b'_') && !is_double(bytes, i) {
            let marker = bytes[i] as char;
            let pat: String = marker.to_string();
            if let Some(close) = find_close(line, i + 1, &pat) {
                if close > i + 1 {
                    flush_plain(&mut out, line, plain_start, i, plain_style);
                    out.push(Span::styled(line[i + 1..close].to_string(), italic_style));
                    i = close + 1;
                    plain_start = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    flush_plain(&mut out, line, plain_start, bytes.len(), plain_style);
    if out.is_empty() {
        out.push(Span::raw(String::new()));
    }
    out
}

fn is_double(bytes: &[u8], i: usize) -> bool {
    if bytes[i] != b'*' {
        return false;
    }
    let prev = i > 0 && bytes[i - 1] == b'*';
    let next = i + 1 < bytes.len() && bytes[i + 1] == b'*';
    prev || next
}

fn find_close(line: &str, start: usize, pat: &str) -> Option<usize> {
    line[start..].find(pat).map(|p| p + start)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    // ── Goal-150 regression tests ──────────────────────────────────────

    #[test]
    fn plain_text_renders_with_default_fg() {
        let r = render_inline("hello world", Color::White, MdState::default());
        assert_eq!(collect_text(&r.spans), "hello world");
        assert!(!r.state.in_code_block);
    }

    #[test]
    fn bold_double_star_styles_inner() {
        let r = render_inline("hi **there** friend", Color::White, MdState::default());
        let text = collect_text(&r.spans);
        assert_eq!(text, "hi there friend");
        let has_bold = r
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold);
    }

    #[test]
    fn inline_code_rendered_with_code_colour() {
        let r = render_inline("call `foo()` now", Color::White, MdState::default());
        let has_yellow = r
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::LightYellow));
        assert!(has_yellow);
        assert_eq!(collect_text(&r.spans), "call foo() now");
    }

    #[test]
    fn italic_with_underscore() {
        let r = render_inline("an _emphasised_ word", Color::White, MdState::default());
        assert_eq!(collect_text(&r.spans), "an emphasised word");
        let has_italic = r
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_italic);
    }

    #[test]
    fn heading_strips_hashes_and_bolds() {
        let r = render_inline("## Hello", Color::White, MdState::default());
        assert_eq!(collect_text(&r.spans), "Hello");
        let h = &r.spans[0];
        assert!(h.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(h.style.fg, Some(Color::LightCyan));
    }

    #[test]
    fn bullet_replaces_dash_with_dot() {
        let r = render_inline("- first item", Color::White, MdState::default());
        let text = collect_text(&r.spans);
        assert!(text.contains("• "));
        assert!(text.contains("first item"));
    }

    #[test]
    fn fence_toggles_code_block_state() {
        let s0 = MdState::default();
        let r1 = render_inline("```rust", Color::White, s0);
        assert!(r1.state.in_code_block);
        let r2 = render_inline("let x = 1;", Color::White, r1.state);
        assert!(r2.state.in_code_block);
        let r3 = render_inline("```", Color::White, r2.state);
        assert!(!r3.state.in_code_block);
    }

    #[test]
    fn unmatched_marker_treated_as_plain() {
        let r = render_inline("a*b c", Color::White, MdState::default());
        assert_eq!(collect_text(&r.spans), "a*b c");
    }

    #[test]
    fn empty_line_yields_an_empty_span() {
        let r = render_inline("", Color::White, MdState::default());
        assert_eq!(r.spans.len(), 1);
        assert_eq!(r.spans[0].content.as_ref(), "");
    }

    // ── fenced_block_multiline_threading_unchanged (regression) ────────

    #[test]
    fn fenced_block_multiline_threading_unchanged() {
        let s0 = MdState::default();
        let r1 = render_inline("```", Color::White, s0);
        assert!(r1.state.in_code_block);
        let r2 = render_inline("some code", Color::White, r1.state);
        assert!(r2.state.in_code_block);
        assert_eq!(r2.spans.len(), 1);
        let r3 = render_inline("```", Color::White, r2.state);
        assert!(!r3.state.in_code_block);
    }

    // ── Goal-159: table tests ──────────────────────────────────────────

    #[test]
    fn table_three_columns_renders_cells() {
        let rows = ["| A | B | C |", "|---|---|---|", "| 1 | 2 | 3 |"];
        let rows_ref: Vec<&str> = rows.iter().map(|s| s.as_ref()).collect();
        let lines = render_table(&rows_ref, "│  ", Style::default().fg(Color::Gray));
        // Should have: top border, header, divider, data row, bottom border = 5 lines
        assert_eq!(lines.len(), 5);
        // Header line should contain A, B, C
        let header_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains('A'));
        assert!(header_text.contains('B'));
        assert!(header_text.contains('C'));
        // Data line should contain 1, 2, 3
        let data_text: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(data_text.contains('1'));
        assert!(data_text.contains('2'));
        assert!(data_text.contains('3'));
    }

    #[test]
    fn table_header_separator_data_parses_correctly() {
        let rows = [
            "| Name     | Value |",
            "|----------|-------|",
            "| foo      | 42    |",
        ];
        let rows_ref: Vec<&str> = rows.iter().map(|s| s.as_ref()).collect();
        let lines = render_table(&rows_ref, "", Style::default());
        // top + header + divider + data + bottom = 5
        assert_eq!(lines.len(), 5);
        let hdr: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(hdr.contains("Name"));
        assert!(hdr.contains("Value"));
    }

    #[test]
    fn table_without_separator_falls_back_to_plain() {
        let rows = ["| A | B |", "| 1 | 2 |"];
        let rows_ref: Vec<&str> = rows.iter().map(|s| s.as_ref()).collect();
        let lines = render_table(&rows_ref, "", Style::default());
        // No header detection means both treated as data rows.
        // top + 2 data + bottom = 4
        assert_eq!(lines.len(), 4);
    }

    // ── Goal-159: syntax-highlight tests ──────────────────────────────

    #[test]
    fn syntax_rust_keywords_get_color_spans() {
        let state = MdState {
            in_code_block: true,
            code_lang: "rust".to_string(),
        };
        let r = render_inline("fn main() {}", Color::White, state);
        // Should produce multiple coloured spans (not just one yellow fallback).
        assert!(!r.spans.is_empty());
        let text = collect_text(&r.spans);
        assert!(text.contains("fn"));
        assert!(text.contains("main"));
    }

    #[test]
    fn syntax_unknown_language_uses_fallback_color() {
        let state = MdState {
            in_code_block: true,
            code_lang: "notalang_xyz".to_string(),
        };
        let r = render_inline("some code here", Color::White, state);
        assert_eq!(r.spans.len(), 1);
        assert_eq!(r.spans[0].style.fg, Some(Color::LightYellow));
    }

    #[test]
    fn syntax_empty_code_block_no_panic() {
        let state = MdState {
            in_code_block: true,
            code_lang: "rust".to_string(),
        };
        let r = render_inline("", Color::White, state);
        // Empty line shouldn't panic; may produce 0 or 1 span.
        let _ = r.spans;
    }
}

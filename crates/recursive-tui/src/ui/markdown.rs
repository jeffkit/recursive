//! Tiny inline-markdown renderer for assistant messages.
//!
//! ratatui doesn't ship a markdown parser, and a full CommonMark
//! pipeline is overkill for chat output. This module implements just
//! the four constructs that come up most in LLM responses:
//!
//! - **bold** (`**...**`)
//! - *italic* / _italic_ (`*...*`, `_..._`)
//! - inline `code` (`` `...` ``)
//! - line-level heading (`# `, `## `, ...) — emitted as a single
//!   bold span with no indent
//! - fenced code block (` ``` ` lines toggle a "code mode" where
//!   subsequent lines are rendered with a code style)
//! - simple bullet (`- ` / `* `) — kept as-is, rendered with a
//!   coloured bullet glyph
//!
//! Anything else is rendered as plain text. The goal is "looks
//! markdown-ish in a 24-bit colour terminal", not "passes
//! CommonMark conformance".
//!
//! Public entry point: [`render_inline`] takes a single line of
//! text and returns a `Vec<Span<'static>>` ready to drop into a
//! `Line`. The caller decides what foreground colour the *plain*
//! text should be (e.g. `Color::White` for assistant messages).
//! Bold/italic/code spans use stronger colours that pop on a black
//! background.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// State carried across consecutive lines so fenced code blocks
/// (` ``` `) can span multiple lines.
#[derive(Default, Clone, Copy)]
pub struct MdState {
    /// True when we're inside a ```...``` fenced code block.
    pub in_code_block: bool,
}

/// Result of rendering a single line.
pub struct RenderedLine {
    pub spans: Vec<Span<'static>>,
    /// Updated state to feed into the next line.
    pub state: MdState,
}

/// Render one logical line of markdown-ish text into styled spans.
///
/// `default_fg` is the colour for plain-text spans (e.g.
/// `Color::White` for the assistant body so it stays bright on
/// black).
pub fn render_inline(line: &str, default_fg: Color, state: MdState) -> RenderedLine {
    // ── fenced code block toggle ──────────────────────────────
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        let new_state = MdState {
            in_code_block: !state.in_code_block,
        };
        // Render the fence line itself dimly so users can see it
        // without it dominating the eye.
        return RenderedLine {
            spans: vec![Span::styled(
                line.to_string(),
                Style::default().fg(Color::Gray),
            )],
            state: new_state,
        };
    }

    if state.in_code_block {
        // Inside a code block: monospace-ish coloured rendering,
        // skip inline parsing.
        return RenderedLine {
            spans: vec![Span::styled(
                line.to_string(),
                Style::default().fg(Color::LightYellow),
            )],
            state,
        };
    }

    // ── heading ────────────────────────────────────────────────
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

    // ── bullet list ────────────────────────────────────────────
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

    // ── plain inline parse ─────────────────────────────────────
    RenderedLine {
        spans: parse_inline(line, default_fg),
        state,
    }
}

/// `# heading` / `## heading` / ... → return the heading body
/// without the leading hashes and required space. Returns `None`
/// when the line is not a heading.
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
    // Require a space after the hashes (avoids matching "#fff" hex).
    if let Some(stripped) = rest.strip_prefix(' ') {
        Some(stripped)
    } else if rest.is_empty() {
        Some("")
    } else {
        None
    }
}

/// Match a leading `- ` / `* ` / `+ ` bullet and return
/// `(indent, body)`. Indent is whitespace before the marker so
/// nested bullets keep their position.
fn strip_bullet(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let indent_len = line.len() - trimmed.len();
    let body = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    Some((&line[..indent_len], body))
}

/// Parse a line for **bold**, *italic*, _italic_, `code` and emit
/// styled spans. Plain text uses `default_fg`. Bold uses
/// `LightCyan + BOLD`, italic uses `Italic + default_fg`, code uses
/// `LightYellow`.
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
                    // non-empty
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
        // empty line
        out.push(Span::raw(String::new()));
    }
    out
}

/// True when the byte at `i` is part of a `**` pair (bold), so we
/// skip it when looking for italic markers.
fn is_double(bytes: &[u8], i: usize) -> bool {
    if bytes[i] != b'*' {
        return false;
    }
    let prev = i > 0 && bytes[i - 1] == b'*';
    let next = i + 1 < bytes.len() && bytes[i + 1] == b'*';
    prev || next
}

/// Find the next occurrence of `pat` starting from `start` (byte
/// offset). Returns the byte offset of the start of the match.
fn find_close(line: &str, start: usize, pat: &str) -> Option<usize> {
    line[start..].find(pat).map(|p| p + start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

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
        // there should be at least one bold span
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
        // inside code block: line should be one yellow span
        assert_eq!(r2.spans.len(), 1);
        assert_eq!(r2.spans[0].style.fg, Some(Color::LightYellow));
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
}

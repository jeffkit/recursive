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

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthStr as _;

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
/// `max_table_width` is the maximum total character width of the rendered
/// table (including gutter). Pass `0` to disable capping.
pub fn render_table(
    rows: &[&str],
    gutter_prefix: &str,
    gutter_style: Style,
    max_table_width: usize,
) -> Vec<Line<'static>> {
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

    // Compute maximum content width per column using Unicode visual width
    // (CJK characters are 2 columns wide; ASCII is 1).
    let mut col_widths: Vec<usize> = vec![0; ncols];
    for (is_sep, cells) in &parsed {
        if *is_sep {
            continue;
        }
        for (ci, cell) in cells.iter().enumerate().take(ncols) {
            col_widths[ci] = col_widths[ci].max(cell.width());
        }
    }
    // Minimum width of 1.
    for w in &mut col_widths {
        if *w == 0 {
            *w = 1;
        }
    }

    // Cap column widths so the table fits within max_table_width.
    // Total visual width = gutter.width() + 1 (left │) + sum(col_width + 3) for each col
    //                    = gutter.width() + 1 + sum(col_widths) + 3*ncols
    if max_table_width > 0 && ncols > 0 {
        let gutter_len = gutter_prefix.width();
        let overhead = gutter_len + 1 + 3 * ncols;
        if overhead < max_table_width {
            let available = max_table_width - overhead;
            let total_content: usize = col_widths.iter().sum();
            if total_content > available {
                // Distribute proportionally; each column keeps at least 1 char.
                for w in &mut col_widths {
                    *w = ((*w * available) / total_content).max(1);
                }
            }
        } else {
            // Extreme case: not enough room even for borders — give each col 1.
            col_widths.fill(1);
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
            // Truncate cell content that exceeds column visual width.
            let (display, vis_w) = truncate_to_visual_width(cell_text, *w);
            // Manual right-padding so CJK chars (width 2) align properly.
            let pad = w.saturating_sub(vis_w);
            let padded = format!(" {}{} ", display, " ".repeat(pad));
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

// ── Goal-172: pulldown-cmark based full-document renderer ─────────────

/// Parse `text` as Markdown and return ratatui [`Line`]s for display.
///
/// Supported elements (minimum viable set):
/// - **Bold** `**text**` → bold style
/// - *Italic* `*text*` → italic style
/// - `Inline code` `` `code` `` → `Color::Cyan`
/// - Fenced code blocks ` ``` ` → each line prefixed with `│ ` in cyan
/// - Unordered lists `- item` / `* item` → prefixed with `• `
/// - Ordered lists `1. item` → prefixed with `N. `
/// - Horizontal rules `---` → a line of `─` chars filling `wrap_width`
/// - Plain paragraphs → rendered as-is
///
/// Falls back to a single raw line per `\n` if parsing produces no
/// output (e.g. an empty string or whitespace-only input).
pub fn render_markdown(text: &str, wrap_width: u16) -> Vec<Line<'static>> {
    let width = if wrap_width == 0 { 80 } else { wrap_width } as usize;

    // Fallback: empty / whitespace-only input.
    if text.trim().is_empty() {
        return text.lines().map(|l| Line::from(l.to_string())).collect();
    }

    let parser = Parser::new_ext(text, Options::ENABLE_TABLES);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut pending_text: String = String::new();
    let mut style_stack: Vec<Style> = vec![Style::default().fg(Color::White)];
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut in_code_block = false;
    let mut code_block_lang: String = String::new();
    let mut code_block_buffer: Vec<String> = Vec::new();
    // Table accumulation state.
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_table_row: Vec<String> = Vec::new();
    let mut current_table_cell = String::new();

    // Helper: append `pending_text` to `current` with the active style.
    let flush_pending =
        |current: &mut Vec<Span<'static>>, pending_text: &mut String, style_stack: &[Style]| {
            if !pending_text.is_empty() {
                let style = style_stack.last().copied().unwrap_or_default();
                current.push(Span::styled(std::mem::take(pending_text), style));
            }
        };

    // Helper: close out the current logical line.
    let flush_line = |out: &mut Vec<Line<'static>>,
                      current: &mut Vec<Span<'static>>,
                      pending_text: &mut String,
                      style_stack: &[Style]| {
        flush_pending(current, pending_text, style_stack);
        if !current.is_empty() {
            out.push(Line::from(std::mem::take(current)));
        }
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    // No-op: paragraphs accumulate inline until End.
                }
                Tag::Strong => {
                    flush_pending(&mut current, &mut pending_text, &style_stack);
                    let base = style_stack.last().copied().unwrap_or_default();
                    style_stack.push(base.add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    flush_pending(&mut current, &mut pending_text, &style_stack);
                    let base = style_stack.last().copied().unwrap_or_default();
                    style_stack.push(base.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    // Flush any pending paragraph line first.
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    in_code_block = true;
                    code_block_lang = match kind {
                        CodeBlockKind::Fenced(s) => s.into_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_block_buffer.clear();
                }
                Tag::List(start) => {
                    // Flush any pending paragraph line first.
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    list_stack.push(ListState {
                        ordered: start.is_some(),
                        next_num: start.unwrap_or(1),
                    });
                }
                Tag::Item => {
                    // Flush any pending paragraph line first.
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    let prefix = if let Some(list) = list_stack.last_mut() {
                        if list.ordered {
                            let n = list.next_num;
                            list.next_num += 1;
                            format!("{}. ", n)
                        } else {
                            "\u{2022} ".to_string()
                        }
                    } else {
                        "\u{2022} ".to_string()
                    };
                    current.push(Span::styled(
                        prefix,
                        Style::default().fg(Color::LightYellow),
                    ));
                }
                Tag::Heading { level, .. } => {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    let heading_style = match level {
                        HeadingLevel::H1 => Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                        HeadingLevel::H2 => Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        HeadingLevel::H3 => Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    };
                    style_stack.push(heading_style);
                }
                Tag::Table(_) => {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    in_table = true;
                    table_rows.clear();
                    current_table_row.clear();
                    current_table_cell.clear();
                }
                Tag::TableHead => {
                    current_table_row.clear();
                    current_table_cell.clear();
                }
                Tag::TableRow => {
                    current_table_row.clear();
                    current_table_cell.clear();
                }
                Tag::TableCell => {
                    current_table_cell.clear();
                }
                _ => {
                    // Tags we don't render specially (links, images, block
                    // quotes, footnote defs, html blocks, def lists,
                    // strikethrough, metadata, math): fall through; their
                    // children become plain text.
                }
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    flush_pending(&mut current, &mut pending_text, &style_stack);
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::CodeBlock => {
                    // Emit each code block line with a `│ ` gutter prefix and
                    // per-token syntax highlighting via syntect.
                    let lang = code_block_lang.clone();
                    for line in code_block_buffer.drain(..) {
                        let mut spans: Vec<Span<'static>> = Vec::new();
                        spans.push(Span::styled(
                            "\u{2502} ".to_string(),
                            Style::default().fg(Color::DarkGray),
                        ));
                        spans.extend(highlight_code_line(&line, &lang));
                        out.push(Line::from(spans));
                    }
                    in_code_block = false;
                    code_block_lang.clear();
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::Item => {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                }
                TagEnd::Heading(_) => {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                    // Pop the level-specific heading style pushed by Tag::Heading.
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::TableCell => {
                    current_table_row.push(std::mem::take(&mut current_table_cell));
                }
                TagEnd::TableHead => {
                    // In pulldown-cmark 0.12 the header cells are NOT wrapped in
                    // a TableRow event — TableHead directly contains TableCells.
                    // Push the accumulated header row, then add the separator row.
                    table_rows.push(std::mem::take(&mut current_table_row));
                    let ncols = table_rows.last().map_or(0, |r| r.len());
                    if ncols > 0 {
                        table_rows.push((0..ncols).map(|_| "---".to_string()).collect());
                    }
                }
                TagEnd::TableRow => {
                    table_rows.push(std::mem::take(&mut current_table_row));
                }
                TagEnd::Table => {
                    // Rebuild raw |-delimited rows and pass them to render_table.
                    let raw_rows: Vec<String> = table_rows
                        .iter()
                        .map(|row| {
                            format!(
                                "|{}|",
                                row.iter()
                                    .map(|c| format!(" {c} "))
                                    .collect::<Vec<_>>()
                                    .join("|")
                            )
                        })
                        .collect();
                    let row_refs: Vec<&str> = raw_rows.iter().map(String::as_str).collect();
                    // `width` already excludes the 2-column indent that the
                    // caller (`render_assistant`) prepends to every line, so
                    // the table can use the full `width` as its budget
                    // (the table's own `"  "` gutter is accounted for inside
                    // `render_table` via `gutter_len`). Width==0 means no
                    // limit.
                    let table_max = width;
                    out.extend(render_table(
                        &row_refs,
                        "  ",
                        Style::default().fg(Color::DarkGray),
                        table_max,
                    ));
                    in_table = false;
                    table_rows.clear();
                    current_table_row.clear();
                    current_table_cell.clear();
                }
                _ => {}
            },
            Event::Text(s) => {
                let text = s.into_string();
                if in_table {
                    // Accumulate plain text into the current table cell.
                    current_table_cell.push_str(&text);
                } else if in_code_block {
                    // Code block text is line-delimited; split on '\n' so
                    // each physical line becomes a separate `Line` later.
                    // We skip a trailing empty element (the natural result
                    // of splitting text that ends in '\n').
                    let mut parts = text.split('\n');
                    if let Some(first) = parts.next() {
                        if !first.is_empty() {
                            code_block_buffer.push(first.to_string());
                        }
                        for part in parts {
                            code_block_buffer.push(part.to_string());
                        }
                    }
                } else {
                    pending_text.push_str(&text);
                }
            }
            Event::Code(s) => {
                if in_table {
                    // Inline code inside a table cell: accumulate as plain text
                    // (table renderer doesn't support per-cell spans).
                    current_table_cell.push('`');
                    current_table_cell.push_str(&s);
                    current_table_cell.push('`');
                } else if in_code_block {
                    // Should not happen in well-formed input.
                    code_block_buffer.push(s.into_string());
                } else {
                    flush_pending(&mut current, &mut pending_text, &style_stack);
                    let base = style_stack.last().copied().unwrap_or_default();
                    let code_style = base.fg(Color::Cyan);
                    current.push(Span::styled(s.into_string(), code_style));
                }
            }
            Event::SoftBreak => {
                if in_table {
                    current_table_cell.push(' ');
                } else if in_code_block {
                    // Treat as hard break inside code blocks.
                    code_block_buffer.push(String::new());
                } else {
                    pending_text.push(' ');
                }
            }
            Event::HardBreak => {
                if in_table {
                    // Table cells are single-line; treat as space.
                    current_table_cell.push(' ');
                } else if in_code_block {
                    code_block_buffer.push(String::new());
                } else {
                    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                }
            }
            Event::Rule => {
                flush_line(&mut out, &mut current, &mut pending_text, &style_stack);
                out.push(Line::from(Span::styled(
                    "\u{2500}".repeat(width),
                    Style::default().fg(Color::Gray),
                )));
            }
            _ => {
                // Html, InlineHtml, InlineMath, DisplayMath, FootnoteReference,
                // TaskListMarker: ignored (rendered as nothing).
            }
        }
    }

    // Flush any trailing content.
    flush_line(&mut out, &mut current, &mut pending_text, &style_stack);

    if out.is_empty() {
        // Parser produced no events (e.g. text was just punctuation or
        // stripped by an option). Fall back to raw lines.
        return text.lines().map(|l| Line::from(l.to_string())).collect();
    }

    out
}

/// State for a single list level, used to render ordered/unordered prefixes.
struct ListState {
    /// True for `1. …`, `2. …`; false for `- …` / `* …` / `+ …`.
    ordered: bool,
    /// Next item number for ordered lists. Ignored for unordered.
    next_num: u64,
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

// ── Unicode visual-width helpers ──────────────────────────────────────

/// Truncate `s` so its Unicode visual width is ≤ `max_width`.
/// Returns `(truncated_string, actual_visual_width)`.
/// If truncation was needed, the last char is replaced with `…`.
fn truncate_to_visual_width(s: &str, max_width: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar as _;
    let mut result = String::new();
    let mut vis_w = 0usize;
    let chars: Vec<char> = s.chars().collect();
    let full_vis = s.width();
    if full_vis <= max_width {
        return (s.to_string(), full_vis);
    }
    // Need to truncate — leave room for the `…` (1 column).
    let budget = max_width.saturating_sub(1);
    for &ch in &chars {
        let cw = ch.width().unwrap_or(0);
        if vis_w + cw > budget {
            break;
        }
        result.push(ch);
        vis_w += cw;
    }
    result.push('…');
    vis_w += 1;
    (result, vis_w)
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

/// Advance the parse cursor by one byte. Extracted as a helper so the
/// `*i += 1` increment can be marked `#[mutants::skip]` — mutating it to
/// `*i *= 1` / `*i -= 1` would hang `parse_inline` in an infinite loop
/// (cargo-mutants timeout), and non-termination cannot be asserted by a
/// passing test. `parse_inline` itself stays fully mutable.
#[cfg_attr(test, mutants::skip)]
#[inline]
fn bump_cursor(i: &mut usize) {
    *i += 1;
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
        bump_cursor(&mut i);
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
        let lines = render_table(&rows_ref, "│  ", Style::default().fg(Color::Gray), 0);
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
        let lines = render_table(&rows_ref, "", Style::default(), 0);
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
        let lines = render_table(&rows_ref, "", Style::default(), 0);
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

    // ── Goal-172: render_markdown tests ────────────────────────────────

    fn lines_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn bold_renders_as_bold_span() {
        let lines = render_markdown("hello **world**!", 80);
        let all_spans: Vec<&Span<'static>> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        let has_bold = all_spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            has_bold,
            "expected at least one bold span; got spans={all_spans:?}"
        );
    }

    #[test]
    fn inline_code_renders_cyan() {
        let lines = render_markdown("call `foo()` now", 80);
        let all_spans: Vec<&Span<'static>> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        let has_cyan = all_spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
        assert!(
            has_cyan,
            "expected at least one cyan span for inline code; got spans={all_spans:?}"
        );
    }

    #[test]
    fn fenced_code_block_prefixed() {
        let src = "```\nsome code\n```";
        let lines = render_markdown(src, 80);
        assert!(!lines.is_empty(), "fenced code block produced no lines");
        // Every emitted line should start with the `│ ` (U+2502) prefix.
        for line in &lines {
            let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            assert!(
                first.starts_with('\u{2502}'),
                "expected fenced code line to start with `\u{2502} `, got {first:?}"
            );
        }
        // The original code text should be present in the output.
        let all_text = lines_text(&lines);
        assert!(
            all_text.contains("some code"),
            "expected code text in output: {all_text:?}"
        );
    }

    #[test]
    fn bullet_list_prefixed() {
        let lines = render_markdown("- first item\n- second", 80);
        let all_text = lines_text(&lines);
        assert!(
            all_text.contains('\u{2022}'),
            "expected bullet `\u{2022}` prefix in list output: {all_text:?}"
        );
        // Each non-empty line should start with `• `.
        for line in &lines {
            let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
            if first.is_empty() {
                continue;
            }
            assert!(
                first.starts_with('\u{2022}'),
                "expected list line to start with `\u{2022} `, got {first:?}"
            );
        }
    }

    #[test]
    fn plain_text_passthrough() {
        let lines = render_markdown("hello world", 80);
        assert!(!lines.is_empty(), "expected at least one line");
        let all_text = lines_text(&lines);
        assert!(
            all_text.contains("hello world"),
            "expected plain text in output: {all_text:?}"
        );
    }

    #[test]
    fn empty_string_returns_empty() {
        let lines = render_markdown("", 80);
        // Empty input: no panic, output is either empty or matches raw lines
        // (which is empty for "").
        assert!(
            lines.is_empty(),
            "expected empty Vec for empty input, got {lines:?}"
        );
    }

    #[test]
    fn zero_wrap_width_falls_back_to_80() {
        // wrap_width = 0 must not panic and must produce output.
        let lines = render_markdown("hello", 0);
        assert!(!lines.is_empty());
    }

    #[test]
    fn horizontal_rule_fills_wrap_width() {
        let lines = render_markdown("---", 40);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let rule_count = text.chars().filter(|c| *c == '\u{2500}').count();
        assert_eq!(
            rule_count, 40,
            "expected 40 box-drawing chars, got {text:?}"
        );
    }

    // Regression: when render_assistant prepends a 2-space indent to every
    // markdown line and then hard-wraps at the panel width, a `---`
    // horizontal rule sized to the full panel width overflows by 2 columns
    // and wraps a stray `─` onto the next row. The caller must reserve the
    // indent by passing `width - 2` to render_markdown; this test simulates
    // that contract so the rule fits exactly within the panel.
    #[test]
    fn horizontal_rule_fits_panel_with_two_space_indent() {
        use crate::ui::transcript::wrap_lines_to_width;
        let panel_width: u16 = 40;
        let indent = Span::raw("  ");
        let md_lines = render_markdown("---", panel_width.saturating_sub(2));
        // Mimic render_assistant: prepend the 2-space indent to every line.
        let indented: Vec<Line<'static>> = md_lines
            .into_iter()
            .map(|l| {
                let mut spans = vec![indent.clone()];
                spans.extend(l.spans);
                Line::from(spans)
            })
            .collect();
        let physical = wrap_lines_to_width(&indented, panel_width);
        // The rule must occupy exactly one physical row — no stray `─`
        // wrapping onto a second line.
        let rule_rows: Vec<&Line<'static>> = physical
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.as_ref().contains('\u{2500}'))
            })
            .collect();
        assert_eq!(
            rule_rows.len(),
            1,
            "horizontal rule wrapped to {} rows; expected 1",
            rule_rows.len()
        );
        let row_text: String = rule_rows[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let rule_count = row_text.chars().filter(|c| *c == '\u{2500}').count();
        assert_eq!(
            rule_count, 38,
            "expected 38 `─` chars (40 - 2 indent), got {rule_count}"
        );
    }
}

/// Inline code `` `x` `` inside a table cell must appear in the rendered
/// output (with surrounding backticks) rather than being swallowed.
#[test]
fn render_markdown_table_with_inline_code_in_cell() {
    let text = "| File | Desc |\n|------|------|\n| `foo.rs` | new file |\n| `bar.rs` | updated |";
    let lines = render_markdown(text, 80);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // Both file names must survive rendering.
    assert!(all_text.contains("foo.rs"), "inline code 'foo.rs' missing");
    assert!(all_text.contains("bar.rs"), "inline code 'bar.rs' missing");
}

/// Verify that render_markdown produces a properly boxed table for a
/// standard GFM table (header + separator + data rows).
#[test]
fn render_markdown_table_end_to_end() {
    let text = "| Name | Value |\n|------|-------|\n| foo | 42 |\n| bar | 99 |";
    let lines = render_markdown(text, 80);
    let all_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // Should contain box-drawing chars and cell content.
    assert!(all_text.contains("Name"), "header 'Name' missing");
    assert!(all_text.contains("Value"), "header 'Value' missing");
    assert!(all_text.contains("foo"), "data 'foo' missing");
    assert!(all_text.contains("42"), "data '42' missing");
    assert!(all_text.contains("bar"), "data 'bar' missing");
    assert!(all_text.contains("99"), "data '99' missing");
    // Box borders should be present.
    assert!(
        all_text.contains('┌') || all_text.contains('─'),
        "no box-drawing chars found"
    );
    // Should be more than 3 lines (top + header + divider + 2 data + bottom = 6).
    assert!(lines.len() >= 6, "expected ≥6 lines, got {}", lines.len());
}

#[cfg(test)]
mod debt_tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    fn line_width(line: &Line<'_>) -> usize {
        unicode_width::UnicodeWidthStr::width(line_text(line).as_str())
    }
    fn all_text(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }
    fn row_refs<'a>(rows: &'a [&'a str]) -> Vec<&'a str> {
        rows.to_vec()
    }

    // ── lazy init (33, 38) ──────────────────────────────────────────────

    #[test]
    fn syntax_set_loads_defaults_not_empty() {
        // kills syntax_set -> Default::default() (33): the default SyntaxSet
        // has no syntaxes; the loaded one has many.
        assert!(syntax_set().syntaxes().len() > 1);
    }

    #[test]
    fn theme_set_loads_defaults_not_empty() {
        // kills theme_set -> Default::default() (38).
        assert!(theme_set().themes.len() > 1);
    }

    // ── is_table_line (290) ──────────────────────────────────────────────

    #[test]
    fn is_table_line_detects_pipe_prefix() {
        // kills is_table_line -> false (290).
        assert!(is_table_line("| a | b |"));
        assert!(is_table_line("  |x|"));
        assert!(!is_table_line("no pipe here"));
    }

    // ── render_table width cap (187, 189, 196) ──────────────────────────

    #[test]
    fn render_table_caps_single_wide_column_to_budget() {
        // ncols=1, gutter="", content width 3, max=6.
        // orig: overhead=0+1+3=4, available=2, cap (3*2)/3=2 -> width 6.
        // Kills: 187 `>`->`<` (skips cap -> width 7), 189:35 `+`->`*`
        // (overhead 3 -> available 3 -> no cap -> width 7), 189:43 `*`->`+`
        // (overhead 5 -> available 1 -> cap 1 -> width 5), 196 `/`->`*`
        // (cap huge -> width huge).
        let rows = ["| xxx |", "|---|", "| yyy |"];
        let lines = render_table(&row_refs(&rows), "", Style::default(), 6);
        assert!(lines.len() >= 4, "expected ≥4 lines, got {}", lines.len());
        // The data row (index 3) width must equal the budget 6.
        let data_w = line_width(&lines[3]);
        assert_eq!(data_w, 6, "data row width {data_w} != budget 6");
    }

    #[test]
    fn render_table_caps_three_columns_division_operator() {
        // ncols=3, gutter="", each col width 3, max=13.
        // orig: overhead=0+1+9=10, available=3, cap (3*3)/9=1 -> width 13.
        // Kills 189:43 `*`->`/`: overhead=0+1+3/3=2 -> available=11 -> cap
        // (3*11)/9=3 -> width 19.
        let rows = [
            "| xxx | xxx | xxx |",
            "|---|---|---|",
            "| yyy | yyy | yyy |",
        ];
        let lines = render_table(&row_refs(&rows), "", Style::default(), 13);
        let data_w = line_width(&lines[3]);
        assert_eq!(data_w, 13, "data row width {data_w} != budget 13");
    }

    #[test]
    fn render_table_header_only_has_no_divider() {
        // kills `-`->`+` in `data_rows.len() - 1` (257): with only a header
        // data row, is_last_data is true -> no divider (3 lines); mutant
        // makes is_last_data always false -> divider printed (4 lines).
        let rows = ["| H |", "|---|"];
        let lines = render_table(&row_refs(&rows), "", Style::default(), 0);
        assert_eq!(
            lines.len(),
            3,
            "expected top+header+bottom, got {} lines",
            lines.len()
        );
    }

    // ── parse_table_rows separator detection (711) ──────────────────────

    #[test]
    fn parse_table_rows_detects_dash_separator() {
        // kills `||`->`&&` (711): non-empty all-dash cells are separators
        // under orig (`false || true`); mutant `&&` gives false.
        let parsed = parse_table_rows(&["|---|---|"]);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].0, "separator row should be flagged is_sep=true");
    }

    // ── render_markdown delete-arm mutants ──────────────────────────────

    #[test]
    fn render_markdown_emphasis_produces_italic() {
        // kills delete Tag::Emphasis arm (366).
        let lines = render_markdown("*italic text*", 80);
        let has_italic = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_italic, "expected an italic span for *italic text*");
    }

    #[test]
    fn render_markdown_ordered_list_numbers_items() {
        // kills delete Tag::List(start) arm (381): without the push, items
        // fall back to the bullet prefix instead of "1. "/"2. ".
        let lines = render_markdown("1. first\n2. second", 80);
        let text = all_text(&lines);
        assert!(text.contains("1."), "expected '1.' prefix; got {text:?}");
        assert!(text.contains("2."), "expected '2.' prefix; got {text:?}");
    }

    #[test]
    fn render_markdown_heading_is_bold() {
        // kills delete Tag::Heading arm (408).
        let lines = render_markdown("# Title", 80);
        let has_bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "expected a bold heading span");
    }

    #[test]
    fn render_markdown_heading_then_paragraph_are_two_lines() {
        // kills delete TagEnd::Heading arm (483): without the flush+pop the
        // heading and paragraph merge into one styled line.
        let lines = render_markdown("# H\nbody", 80);
        assert!(
            lines.len() >= 2,
            "expected heading and body on separate lines, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn render_markdown_nested_list_outer_bullet_after_inner() {
        // kills delete TagEnd::List arm (477): without popping the inner
        // (ordered) list, the following outer item inherits the inner's
        // ListState and gets a "3." prefix instead of the "• " bullet.
        // orig: "1." "2." (inner) then "• " (outer 'after'); mutant: "3. ".
        let lines = render_markdown("- outer\n  1. inner1\n  2. inner2\n- after", 80);
        let text = all_text(&lines);
        assert!(
            text.contains('\u{2022}'),
            "expected a bullet for the outer 'after' item; got {text:?}"
        );
        assert!(
            !text.contains("3."),
            "outer 'after' should NOT inherit the inner ordered counter; got {text:?}"
        );
    }

    // ── render_markdown: soft/hard break + paragraph flush (452, 581, 591) ─

    #[test]
    fn render_markdown_soft_break_inserts_space() {
        // kills delete Event::SoftBreak arm (581): orig joins lines with a
        // space; mutant drops the space -> "helloworld".
        let lines = render_markdown("hello\nworld", 80);
        let text = all_text(&lines);
        assert!(
            text.contains("hello world"),
            "expected soft break to insert a space; got {text:?}"
        );
    }

    #[test]
    fn render_markdown_hard_break_flushes_line() {
        // kills delete Event::HardBreak arm (591): orig flushes -> 2 lines;
        // mutant (no flush) -> 1 line.
        let lines = render_markdown("line1  \nline2", 80);
        assert!(
            lines.len() >= 2,
            "expected hard break to split into 2 lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn render_markdown_two_paragraphs_are_two_lines() {
        // kills delete TagEnd::Paragraph arm (452): orig flushes between
        // paragraphs -> 2 lines; mutant -> 1 merged line.
        let lines = render_markdown("para1\n\npara2", 80);
        assert!(
            lines.len() >= 2,
            "expected two paragraphs on separate lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn render_markdown_list_then_paragraph_are_separate_lines() {
        // kills delete TagEnd::Item arm (480): the item's End flush is the
        // only flush between a list and a following paragraph (the next
        // Item-start flush doesn't fire). orig -> "• a" + "para" (2 lines);
        // mutant -> "• apara" (1 line).
        let lines = render_markdown("- a\n\npara", 80);
        assert!(
            lines.len() >= 2,
            "expected list item and paragraph on separate lines, got {} lines: {lines:?}",
            lines.len()
        );
    }

    // ── render_markdown: heading level colours (411, 414, 417) ──────────

    fn has_fg(lines: &[Line<'_>], fg: Color) -> bool {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(fg))
    }

    #[test]
    fn render_markdown_h1_is_light_cyan() {
        // kills delete HeadingLevel::H1 arm (411): mutant falls to `_ =>`
        // (Blue) so H1 is no longer LightCyan.
        let lines = render_markdown("# H1", 80);
        assert!(has_fg(&lines, Color::LightCyan), "H1 should be LightCyan");
    }

    #[test]
    fn render_markdown_h2_is_cyan() {
        // kills delete HeadingLevel::H2 arm (414).
        let lines = render_markdown("## H2", 80);
        assert!(has_fg(&lines, Color::Cyan), "H2 should be Cyan");
    }

    #[test]
    fn render_markdown_h3_is_light_blue() {
        // kills delete HeadingLevel::H3 arm (417).
        let lines = render_markdown("### H3", 80);
        assert!(has_fg(&lines, Color::LightBlue), "H3 should be LightBlue");
    }

    // ── render_markdown: style pop on end (455, 457, 486) ───────────────

    #[test]
    fn render_markdown_bold_style_does_not_persist_past_end() {
        // kills delete TagEnd::Strong|Emphasis (455) and `>`->`==`/`<` in
        // the `len > 1` pop guard (457): without popping, the bold style
        // leaks onto the following plain text.
        let lines = render_markdown("**b** plain", 80);
        let plain_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref().contains("plain"));
        let span = plain_span.expect("a span containing 'plain' must exist");
        assert!(
            !span.style.add_modifier.contains(Modifier::BOLD),
            "'plain' should not be bold; got {span:?}"
        );
    }

    #[test]
    fn render_markdown_heading_style_does_not_persist_past_end() {
        // kills `>`->`==`/`<` in the heading `len > 1` pop guard (486):
        // without popping, the LightCyan heading style leaks onto "body".
        let lines = render_markdown("# H\nbody", 80);
        let body_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref().contains("body"));
        let span = body_span.expect("a span containing 'body' must exist");
        assert_ne!(
            span.style.fg,
            Some(Color::LightCyan),
            "'body' should not inherit the heading colour; got {span:?}"
        );
    }

    // ── parse_table_rows: alignment separator (711:84) ──────────────────

    #[test]
    fn parse_table_rows_detects_alignment_separator() {
        // kills the inner `||`->`&&` (711:84): a `:`-alignment separator is
        // detected under orig; mutant makes `:` chars fail the dash check.
        let parsed = parse_table_rows(&["|:--:|:--:|"]);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].0, "alignment separator should be flagged is_sep");
    }

    // ── make_border_line: border width + separator count (733, 736) ─────

    #[test]
    fn render_table_top_border_has_correct_width_and_separators() {
        // 3 cols, each width 3 (content "xxx"), gutter "", max=0 (no cap).
        // top border width = 1 + 3*(3+2) + 2 sep + 1 = 1+15+2+1 = 19.
        // sep count (┬) = ncols-1 = 2.
        // Kills 733 `+`->`*` (w+2 -> w*2 -> wider), 736:14 `<`->`==`/`>`/`<=`
        // and 736:33 `-`->`+`/`/` (sep count changes).
        let rows = [
            "| xxx | xxx | xxx |",
            "|---|---|---|",
            "| yyy | yyy | yyy |",
        ];
        let lines = render_table(&row_refs(&rows), "", Style::default(), 0);
        let top = line_text(&lines[0]);
        let top_w = unicode_width::UnicodeWidthStr::width(top.as_str());
        assert_eq!(top_w, 19, "top border width {top_w} != 19: {top:?}");
        let sep_count = top.chars().filter(|c| *c == '┬').count();
        assert_eq!(sep_count, 2, "expected 2 ┬ separators, got {top:?}");
    }

    // ── truncate_to_visual_width (763) ──────────────────────────────────

    #[test]
    fn truncate_to_visual_width_budget_boundary() {
        // kills `>`->`>=` in `vis_w + cw > budget` (763): "abcdef" with
        // max_width 3 -> budget 2. orig: 'a'(1),'b'(2) then 'c' 2+1>2 break
        // -> "ab…"; mutant `>=`: 'b' 1+1>=2 break -> "a…".
        let (out, w) = truncate_to_visual_width("abcdef", 3);
        assert_eq!(out, "ab…");
        assert_eq!(w, 3);
    }

    // ── is_double: next-non-star (880:33) ───────────────────────────────

    #[test]
    fn is_double_minus_one_index_detects_prev_star() {
        // kills `-`->`/` in `bytes[i - 1]` (880:33): mutant reads bytes[i]
        // (the char itself, always '*' in this branch) so any '*' at i>0 is
        // treated as double. "a*b*": orig italicises "b"; mutant skips.
        let spans = parse_inline("a*b*", Color::White);
        assert!(
            italic_count(&spans) >= 1,
            "expected italic for 'b' in 'a*b*'; got spans={spans:?}"
        );
    }

    // ── syntect_color_to_ratatui (695) ──────────────────────────────────

    #[test]
    fn syntect_color_to_ratatui_maps_rgb() {
        // kills -> Default::default() (695).
        let style = SyntectStyle {
            foreground: syntect::highlighting::Color {
                r: 10,
                g: 20,
                b: 30,
                a: 255,
            },
            ..Default::default()
        };
        assert_eq!(syntect_color_to_ratatui(style), Color::Rgb(10, 20, 30));
    }

    // ── strip_heading (781, 783) ────────────────────────────────────────

    #[test]
    fn strip_heading_seven_hashes_is_none() {
        // kills `<`->`<=` (781): 7 hashes -> orig stops at 6 then fails the
        // space check (rest="# x") -> None; mutant loops a 7th time -> Some.
        assert_eq!(strip_heading("####### x"), None);
    }

    #[test]
    fn strip_heading_single_hash_strips() {
        // kills `+=`->`-=` (783): underflow on the first increment either
        // panics (caught) or wraps, diverging from Some("x").
        assert_eq!(strip_heading("# x"), Some("x"));
        assert_eq!(strip_heading("## y"), Some("y"));
    }

    // ── parse_inline (827, 858) ─────────────────────────────────────────

    #[test]
    fn parse_inline_bold_at_start_has_no_leading_empty_span() {
        // kills `>`->`>=` in flush_plain (827): at i=0 with bold found,
        // flush_plain(0,0) -> orig `0>0` false (no empty span); mutant
        // `0>=0` true (pushes an empty leading span).
        let spans = parse_inline("**bold**", Color::White);
        assert!(!spans.is_empty());
        assert!(
            !spans[0].content.as_ref().is_empty(),
            "first span should be the bold span, not empty; got {spans:?}"
        );
        assert_eq!(spans[0].content.as_ref(), "bold");
    }

    #[test]
    fn parse_inline_double_underscore_stays_plain() {
        // kills `>`->`>=` in `close > i + 1` (858): "__" -> orig skips
        // (close==i+1) keeping "a__b" plain; mutant treats it as empty
        // italic, dropping the underscores -> text "ab".
        let spans = parse_inline("a__b", Color::White);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a__b");
    }

    // ── is_double (877, 880) ────────────────────────────────────────────

    fn italic_count(spans: &[Span<'_>]) -> usize {
        spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::ITALIC))
            .count()
    }

    #[test]
    fn is_double_prevents_star_part_of_double_as_italic() {
        // kills is_double -> false (877:5): "*a**b*" -> orig one italic span
        // ("a"); mutant (always false) also italicises "b" -> two italic
        // spans.
        let spans = parse_inline("*a**b*", Color::White);
        assert_eq!(italic_count(&spans), 1, "spans={spans:?}");
    }

    #[test]
    fn is_double_underscore_adjacent_to_star_is_not_double() {
        // kills `!=`->`==` (877:17): "_*x*_" -> orig opens italic on the
        // leading "_" (is_double false for non-'*') -> italic span; mutant
        // returns true for the "_" next to "*" -> italic skipped.
        let spans = parse_inline("_*x*_", Color::White);
        assert!(
            italic_count(&spans) >= 1,
            "expected italic for '_*x*_' ; got spans={spans:?}"
        );
    }

    #[test]
    fn is_double_prev_star_detected() {
        // kills `>`->`<` in `i > 0` (880): "a**b*" -> orig keeps "a**b*"
        // plain (the '*' at index 2 has a preceding '*'); mutant makes
        // `prev` always false, so that '*' opens an italic, dropping a '*'.
        let spans = parse_inline("a**b*", Color::White);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a**b*");
    }
}

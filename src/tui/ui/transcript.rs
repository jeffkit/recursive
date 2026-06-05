//! Block-aware transcript renderer.
//!
//! Goal-144 replaces the single-line `StyledMessage::to_line()` ladder
//! with one render function per [`TranscriptBlock`] variant. A block
//! produces 1-or-more [`Line`]s; the chat panel concatenates the
//! results separated by a blank line.

use ratatui::prelude::*;

use crate::tui::app::{ToolResultData, TranscriptBlock, UsageStats};
use crate::tui::ui::theme::Theme;
use crate::tui::ui::{diff, markdown};

/// Convert the entire transcript into a flat `Vec<Line>` with one
/// blank line between adjacent blocks. Folded ToolResult blocks
/// honour the `expanded` flag.
///
/// `width` is the available render width in columns (0 = no limit / 80-char
/// fallback). Passed down to the markdown renderer so tables and code blocks
/// can adapt to the actual terminal width.
pub fn render_blocks(
    blocks: &[TranscriptBlock],
    _usage: &UsageStats,
    th: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            // Always insert one blank line between consecutive blocks.
            lines.push(Line::raw(""));
            // Add a second blank line before User blocks so the conversation
            // has clear visual breathing room between turns.
            if matches!(b, TranscriptBlock::User { .. }) {
                lines.push(Line::raw(""));
            }
        }
        lines.extend(render_block(b, th, width));
        // Add a trailing blank line after each User block so the AI response
        // that follows feels spacious (same two-line gap on both sides).
        if matches!(b, TranscriptBlock::User { .. }) {
            lines.push(Line::raw(""));
        }
    }
    lines
}

/// Render a single block. Exposed for unit tests.
///
/// `width` is the available render width in columns (0 = no limit).
pub fn render_block(block: &TranscriptBlock, th: &Theme, width: u16) -> Vec<Line<'static>> {
    match block {
        TranscriptBlock::User { text } => render_user(text, th),
        TranscriptBlock::Assistant {
            text,
            streaming,
            latency_ms,
        } => render_assistant(text, *streaming, *latency_ms, th, width),
        TranscriptBlock::Reasoning { text } => render_reasoning(text),
        TranscriptBlock::ToolCall {
            name,
            args_preview,
            result,
            ..
        } => render_tool_call(name, args_preview, result, th),
        TranscriptBlock::Diff { path, hunks } => render_diff(path, hunks),
        TranscriptBlock::Compacted { removed, kept } => render_compacted(*removed, *kept),
        TranscriptBlock::System { text } => render_system(text),
        TranscriptBlock::Error { text } => render_error(text, th),
        TranscriptBlock::PlanProposal {
            plan_text,
            tool_calls,
        } => render_plan_proposal(plan_text, tool_calls),
        TranscriptBlock::PlanModeRequest { reason, approved } => {
            render_plan_mode_request(reason, *approved)
        }
        #[cfg(feature = "weixin")]
        TranscriptBlock::WeixinMessage { user_id, text } => render_weixin_message(user_id, text),
    }
}

#[cfg(feature = "weixin")]
fn render_weixin_message(user_id: &str, text: &str) -> Vec<Line<'static>> {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };
    let prefix_style = Style::default()
        .fg(Color::Rgb(0, 190, 100))
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(Color::White);

    let prefix = Span::styled(format!("📱 {user_id}: "), prefix_style);
    let mut out: Vec<Line<'static>> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        out.push(Line::from(vec![prefix]));
        return out;
    }
    let first = lines[0];
    out.push(Line::from(vec![
        prefix.clone(),
        Span::styled(first.to_string(), body_style),
    ]));
    for line in &lines[1..] {
        let indent = " ".repeat(user_id.len() + 6); // align continuation lines
        out.push(Line::from(vec![
            Span::styled(indent, Style::default()),
            Span::styled(line.to_string(), body_style),
        ]));
    }
    out
}

// ── User ──────────────────────────────────────────────────────────────

/// Render a user message in Claude-Code style: `> text` with a
/// white foreground on a dim grey background highlight. Multi-line
/// messages stay stacked, one `>` per line, so the visual shape
/// mirrors the indentation of a quoted block reply.
fn render_user(text: &str, _th: &Theme) -> Vec<Line<'static>> {
    let body_bg = Color::Rgb(50, 50, 55);
    let prefix_style = Style::default()
        .fg(Color::DarkGray)
        .bg(body_bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().bg(body_bg).fg(Color::White);

    let prefix = Span::styled("> ", prefix_style);
    let mut out: Vec<Line<'static>> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    if lines.is_empty() {
        // Empty placeholder: a bare `> ` with a single background
        // pixel so the row still picks up the highlight.
        out.push(Line::from(vec![
            prefix,
            Span::styled(" ".to_string(), body_style),
        ]));
        return out;
    }

    for line in lines {
        out.push(Line::from(vec![
            prefix.clone(),
            Span::styled(format!(" {line}"), body_style),
        ]));
    }
    out
}

// ── Assistant ─────────────────────────────────────────────────────────

/// Render an assistant message in Claude-Code style: one cyan
/// bullet, then a `  ` indent for the body. The legacy "▎ Agent"
/// header, inline `⏱` latency, and `…streaming` text are removed;
/// streaming and latency are now expressed through the in-flight
/// bullet colour and the status bar respectively. Empty text
/// collapses to a bare bullet.
fn render_assistant(
    text: &str,
    _streaming: bool,
    _latency_ms: Option<u64>,
    _th: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let bullet_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let indent = Span::raw("  ");
    let mut out: Vec<Line<'static>> = Vec::new();

    if text.is_empty() {
        out.push(Line::from(vec![Span::styled("•", bullet_style), indent]));
        return out;
    }

    // Use the full pulldown-cmark based renderer for proper markdown
    // parsing (handles multi-paragraph blocks, nested lists, fenced
    // code with syntax highlighting, etc.). The first line picks up
    // the bullet prefix; subsequent lines share the 2-space indent
    // so wrapping reads naturally.
    let md_lines = markdown::render_markdown(text, width);
    let mut iter = md_lines.into_iter();
    if let Some(first) = iter.next() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(first.spans.len() + 2);
        spans.push(Span::styled("•", bullet_style));
        spans.push(indent.clone());
        spans.extend(first.spans);
        out.push(Line::from(spans));
    } else {
        // Empty markdown output (only whitespace): a bare bullet.
        out.push(Line::from(vec![
            Span::styled("•", bullet_style),
            indent.clone(),
        ]));
        return out;
    }
    for line in iter {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
        spans.push(indent.clone());
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
}

// ── Reasoning / thinking ──────────────────────────────────────────────

/// Render a reasoning / thinking block in Claude-Code style:
/// a `∴ Thinking…` header in dim yellow, followed by the
/// reasoning text in a slightly muted gray. Empty / whitespace-only
/// reasoning collapses to just the header.
fn render_reasoning(text: &str) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    // Slightly muted gray-white so thinking content is visually distinct
    // from regular assistant text (which is pure white).
    let body_style = Style::default()
        .fg(Color::Rgb(170, 170, 170))
        .add_modifier(Modifier::ITALIC);
    let indent = Span::raw("  ");

    let mut out = vec![Line::from(vec![
        indent,
        Span::styled("∴ Thinking…".to_string(), header_style),
    ])];

    for line in text.lines() {
        if line.is_empty() {
            out.push(Line::raw(""));
        } else {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line.to_string(), body_style),
            ]));
        }
    }

    out
}

// ── ToolCall (with paired result) ─────────────────────────────────────

/// Render a tool call in Claude-Code style: one `⏺` bullet whose
/// colour reflects the tool state, followed by the args in a
/// function-call style (`name(args)`). Below the bullet sits an
/// indented result block (`⎿`) when the tool has finished; while
/// the tool is still running, the result line shows `Running…`.
///
/// States:
/// - `result == None`        → bullet Yellow, body `Running…`
/// - `result.success == true` → bullet Green,  body output (collapsed/expanded)
/// - `result.success == false`→ bullet Red,    body output in error colour
fn render_tool_call(
    name: &str,
    args_preview: &str,
    result: &Option<ToolResultData>,
    th: &Theme,
) -> Vec<Line<'static>> {
    let bullet_color = match result {
        None => Color::Yellow,
        Some(ToolResultData { success: false, .. }) => Color::Red,
        Some(_) => Color::Green,
    };
    let body_color = match result {
        Some(ToolResultData { success: false, .. }) => th.tool_err_fg,
        _ => th.status_fg,
    };
    let size = result
        .as_ref()
        .map(|r| format_size(r.output.len()))
        .unwrap_or_default();
    let args_display = if args_preview.is_empty() {
        String::new()
    } else {
        format!("({args_preview})")
    };

    let mut out = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "⏺".to_string(),
            Style::default()
                .fg(bullet_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(bullet_color)
                .add_modifier(Modifier::BOLD),
        ),
        // Args preview shares the body color so the whole tool line
        // reads in one tone; the bullet and tool name still pop in
        // the status colour.
        Span::styled(args_display, Style::default().fg(body_color)),
    ])];

    match result {
        None => {
            out.push(Line::from(vec![
                Span::styled("    ⎿  ", Style::default().fg(body_color)),
                Span::styled(
                    "Running…".to_string(),
                    Style::default()
                        .fg(body_color)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        Some(ToolResultData {
            success: _,
            output,
            expanded,
        }) => {
            if !size.is_empty() {
                out.push(Line::from(vec![
                    Span::styled("    ⎿  ", Style::default().fg(body_color)),
                    Span::styled(
                        size,
                        Style::default()
                            .fg(body_color)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
            let collected: Vec<&str> = output.lines().collect();
            let n = collected.len();
            let visible: Vec<&&str> = if *expanded || n <= 6 {
                collected.iter().collect()
            } else {
                collected.iter().take(3).collect()
            };
            for line in visible {
                out.push(Line::from(vec![
                    Span::styled("    ⎿  ", Style::default().fg(body_color)),
                    Span::styled((*line).to_string(), Style::default().fg(body_color)),
                ]));
            }
            if !*expanded && n > 6 {
                out.push(Line::from(vec![
                    Span::styled("    ⎿  ", Style::default().fg(body_color)),
                    Span::styled(
                        format!("… ({} more lines, press Ctrl+E to expand)", n - 3),
                        Style::default()
                            .fg(body_color)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
        }
    }

    out
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ── Diff ──────────────────────────────────────────────────────────────

fn render_diff(path: &str, hunks: &[crate::tui::app::DiffHunk]) -> Vec<Line<'static>> {
    let mut out = vec![diff::header_line(path)];
    if hunks.is_empty() {
        out.push(diff::empty_stub_line(path));
    } else {
        out.extend(diff::body_lines(hunks));
    }
    out
}

// ── Compacted / System / Error ────────────────────────────────────────

fn render_compacted(removed: usize, kept: usize) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("⊕", Style::default().fg(Color::Gray)),
        Span::raw(" "),
        Span::styled(
            format!("Conversation compacted: {removed} messages → {kept} summary"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ),
    ])]
}

fn render_system(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        text.to_string(),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
    )])]
}

fn render_error(text: &str, th: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        text.to_string(),
        Style::default().fg(th.tool_err_fg),
    )])]
}

// ── PlanProposal ──────────────────────────────────────────────────────

/// Render a plan proposal inline in the transcript.
///
/// Layout:
/// ```text
/// ╔ ⚡ Plan Proposal ──────────────────╗
/// ║ <plan_text, line by line>          ║
/// ║                                    ║
/// ║ Pending tools (N):                 ║
/// ║   • tool_name(args_preview)        ║
/// ║                                    ║
/// ║ [y/Enter] Approve  [n] Reject  [e] Edit
/// ╚────────────────────────────────────╝
/// ```
fn render_plan_proposal(plan_text: &str, tool_calls: &[serde_json::Value]) -> Vec<Line<'static>> {
    let border = Style::default().fg(Color::Cyan);
    let header = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Cyan);
    let tool_name_style = Style::default().fg(Color::Yellow);

    let mut out: Vec<Line<'static>> = Vec::new();

    // Top border + title
    out.push(Line::from(vec![
        Span::styled("┌─ ", border),
        Span::styled("⚡ Plan Proposal ", header),
        Span::styled("─────────────────────────────────────", border),
    ]));

    // Plan text body
    for raw in plan_text.lines() {
        out.push(Line::from(vec![
            Span::styled("│ ", border),
            Span::styled(raw.to_string(), body),
        ]));
    }

    // Separator before tool list
    out.push(Line::from(vec![Span::styled("│", border)]));
    out.push(Line::from(vec![
        Span::styled("│ ", border),
        Span::styled(
            format!("Pending tools ({}):", tool_calls.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if tool_calls.is_empty() {
        out.push(Line::from(vec![
            Span::styled("│ ", border),
            Span::styled("  (none)", dim),
        ]));
    } else {
        for tc in tool_calls {
            let name = tc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let args = tc
                .get("arguments")
                .map(|v| plan_args_preview(v, 50))
                .unwrap_or_default();
            out.push(Line::from(vec![
                Span::styled("│  • ", border),
                Span::styled(name.to_string(), tool_name_style),
                Span::styled(format!("({args})"), body),
            ]));
        }
    }

    // Action hint row
    out.push(Line::from(vec![Span::styled("│", border)]));
    out.push(Line::from(vec![
        Span::styled("│  ", border),
        Span::styled("[y/Enter] ", key),
        Span::styled(
            "Approve",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("[n/Esc] ", key),
        Span::styled(
            "Reject",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("[e] ", key),
        Span::styled("Edit", Style::default().fg(Color::Yellow)),
    ]));

    // Bottom border
    out.push(Line::from(vec![Span::styled(
        "└─────────────────────────────────────────────────────────",
        border,
    )]));

    out
}

/// Compact preview of a tool's `arguments` JSON (max `limit` chars).
fn plan_args_preview(value: &serde_json::Value, limit: usize) -> String {
    use serde_json::Value;
    let raw = match value {
        Value::String(s) => format!("\"{s}\""),
        Value::Object(map) => {
            let mut parts = Vec::new();
            for (k, v) in map.iter().take(2) {
                let v_str = match v {
                    Value::String(s) => {
                        let s = if s.chars().count() > 20 {
                            let h: String = s.chars().take(19).collect();
                            format!("{h}…")
                        } else {
                            s.clone()
                        };
                        format!("\"{s}\"")
                    }
                    other => other.to_string(),
                };
                parts.push(format!("{k}={v_str}"));
            }
            parts.join(", ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if raw.chars().count() > limit {
        let head: String = raw.chars().take(limit - 1).collect();
        format!("{head}…")
    } else {
        raw
    }
}

// ── PlanModeRequest (Goal-202) ────────────────────────────────────────

/// Render the inline plan-mode entry request block:
///
/// ```text
/// ╔─ ⓘ Plan Mode Request ────────────────╗
/// ║ Agent wants to enter plan mode:       ║
/// ║                                       ║
/// ║   <reason>                            ║
/// ║                                       ║
/// ║ Allow agent to explore and plan?      ║
/// ║  [y/Enter] Allow   [n/Esc] Skip       ║
/// ╚───────────────────────────────────────╝
/// ```
///
/// After decision: shows `✓ Plan mode allowed` or `✗ Plan mode skipped`.
fn render_plan_mode_request(reason: &str, approved: Option<bool>) -> Vec<Line<'static>> {
    let border = Style::default().fg(Color::Blue);
    let header_style = Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);
    let key = Style::default().fg(Color::Cyan);

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::raw(""));

    match approved {
        Some(true) => {
            out.push(Line::from(vec![
                Span::styled("┌─ ", border),
                Span::styled(
                    "✓ Plan mode allowed",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ─────────────────────────────────────────", border),
            ]));
            out.push(Line::from(vec![
                Span::styled("│  ", border),
                Span::styled(reason.to_owned(), Style::default().fg(Color::DarkGray)),
            ]));
            out.push(Line::from(vec![Span::styled(
                "└─────────────────────────────────────────────────────",
                border,
            )]));
        }
        Some(false) => {
            out.push(Line::from(vec![
                Span::styled("┌─ ", border),
                Span::styled(
                    "✗ Plan mode skipped",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ─────────────────────────────────────────", border),
            ]));
            out.push(Line::from(vec![
                Span::styled("│  ", border),
                Span::styled(reason.to_owned(), Style::default().fg(Color::DarkGray)),
            ]));
            out.push(Line::from(vec![Span::styled(
                "└─────────────────────────────────────────────────────",
                border,
            )]));
        }
        None => {
            // Pending — show full request UI.
            out.push(Line::from(vec![
                Span::styled("┌─ ", border),
                Span::styled("ⓘ Plan Mode Request", header_style),
                Span::styled(" ─────────────────────────────────────────", border),
            ]));
            out.push(Line::from(vec![
                Span::styled("│  ", border),
                Span::styled(
                    "Agent wants to enter plan mode:",
                    body.add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![Span::styled("│", border)]));
            for line in reason.lines() {
                out.push(Line::from(vec![
                    Span::styled("│    ", border),
                    Span::styled(line.to_owned(), Style::default().fg(Color::Yellow)),
                ]));
            }
            out.push(Line::from(vec![Span::styled("│", border)]));
            out.push(Line::from(vec![
                Span::styled("│  ", border),
                Span::styled("Allow agent to explore and create a plan?", body),
            ]));
            out.push(Line::raw("│"));
            out.push(Line::from(vec![
                Span::styled("│   ", border),
                Span::styled("[y/Enter] ", key),
                Span::styled(
                    "Allow",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("[n/Esc] ", key),
                Span::styled("Skip — execute directly", Style::default().fg(Color::Red)),
            ]));
            out.push(Line::from(vec![Span::styled(
                "└─────────────────────────────────────────────────────",
                border,
            )]));
        }
    }

    out.push(Line::raw(""));
    out
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{DiffHunk, DiffLine, DiffLineKind, ToolResultData, TranscriptBlock};
    use crate::tui::ui::theme;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn full_text(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn user_block_renders_quote_prefix_and_body() {
        let lines = render_block(
            &TranscriptBlock::User {
                text: "hello world".into(),
            },
            &theme::DARK,
            0,
        );
        let txt = line_text(&lines[0]);
        assert!(txt.starts_with("> "), "first line should start with `> `");
        assert!(txt.contains("hello world"));
    }

    #[test]
    fn user_block_multiline_stacks_with_quote_prefix() {
        let lines = render_block(
            &TranscriptBlock::User {
                text: "line1\nline2".into(),
            },
            &theme::DARK,
            0,
        );
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).starts_with("> "));
        assert!(line_text(&lines[1]).starts_with("> "));
        assert!(line_text(&lines[1]).contains("line2"));
    }

    #[test]
    fn user_block_carries_background_highlight() {
        let lines = render_block(
            &TranscriptBlock::User { text: "hi".into() },
            &theme::DARK,
            0,
        );
        let has_bg = lines[0]
            .spans
            .iter()
            .any(|s| s.style.bg.is_some() && s.style.bg != Some(Color::Reset));
        assert!(has_bg, "user message should have a background highlight");
    }

    #[test]
    fn reasoning_block_has_thinking_header_and_italic_body() {
        let lines = render_block(
            &TranscriptBlock::Reasoning {
                text: "let me think about this\nmaybe this way".into(),
            },
            &theme::DARK,
            0,
        );
        let header = line_text(&lines[0]);
        assert!(
            header.contains("Thinking") || header.contains("∴"),
            "missing thinking header"
        );
        let body = full_text(&lines);
        assert!(body.contains("let me think about this"));
        assert!(body.contains("maybe this way"));
        // body lines should be italic
        let has_italic = lines[1..]
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(has_italic, "reasoning body should be italic");
    }

    #[test]
    fn reasoning_block_empty_text_still_shows_header() {
        let lines = render_block(
            &TranscriptBlock::Reasoning {
                text: String::new(),
            },
            &theme::DARK,
            0,
        );
        let h = line_text(&lines[0]);
        assert!(h.contains("Thinking") || h.contains("∴"));
    }

    #[test]
    fn assistant_block_renders_bullet_no_label() {
        let lines = render_block(
            &TranscriptBlock::Assistant {
                text: "hello".into(),
                streaming: false,
                latency_ms: None,
            },
            &theme::DARK,
            0,
        );
        let txt = line_text(&lines[0]);
        assert!(txt.contains("•"), "assistant must lead with a bullet");
        assert!(!txt.contains("Agent"), "old `Agent` label should be gone");
        assert!(!txt.contains("⏱"), "latency should not be in the block");
        assert!(!txt.contains("streaming"), "no streaming label anymore");
    }

    #[test]
    fn tool_call_without_result_shows_running_in_yellow() {
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "Read".into(),
                args_preview: "path=\"foo\"".into(),
                result: None,
            },
            &theme::DARK,
            0,
        );
        let header = &lines[0];
        assert!(line_text(header).contains("⏺"));
        assert!(line_text(header).contains("Read"));
        // Bullet must be yellow.
        let bullet_yellow = header
            .spans
            .iter()
            .any(|s| s.content == "⏺" && s.style.fg == Some(Color::Yellow));
        assert!(bullet_yellow, "running bullet should be yellow");
        // Second line should be the Running… placeholder.
        assert!(line_text(&lines[1]).contains("Running"));
    }

    #[test]
    fn tool_call_with_successful_result_turns_bullet_green() {
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "Read".into(),
                args_preview: String::new(),
                result: Some(ToolResultData {
                    success: true,
                    output: "abc".into(),
                    expanded: false,
                }),
            },
            &theme::DARK,
            0,
        );
        let header = &lines[0];
        let bullet_green = header
            .spans
            .iter()
            .any(|s| s.content == "⏺" && s.style.fg == Some(Color::Green));
        assert!(bullet_green, "successful bullet should be green");
        assert!(full_text(&lines).contains("abc"));
    }

    #[test]
    fn tool_call_with_failed_result_turns_bullet_red() {
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "x".into(),
                args_preview: String::new(),
                result: Some(ToolResultData {
                    success: false,
                    output: "boom".into(),
                    expanded: false,
                }),
            },
            &theme::DARK,
            0,
        );
        let header = &lines[0];
        let has_red = header.spans.iter().any(|s| s.style.fg == Some(Color::Red));
        assert!(has_red, "failed bullet should be red");
    }

    #[test]
    fn tool_result_long_output_truncated_with_hint() {
        let output = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "Read".into(),
                args_preview: String::new(),
                result: Some(ToolResultData {
                    success: true,
                    output,
                    expanded: false,
                }),
            },
            &theme::DARK,
            0,
        );
        let txt = full_text(&lines);
        assert!(txt.contains("Ctrl+E"));
        assert!(txt.contains("more lines"));
    }

    #[test]
    fn tool_result_expanded_shows_all() {
        let output = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "Read".into(),
                args_preview: String::new(),
                result: Some(ToolResultData {
                    success: true,
                    output,
                    expanded: true,
                }),
            },
            &theme::DARK,
            0,
        );
        for i in 0..10 {
            assert!(full_text(&lines).contains(&format!("line {i}")));
        }
    }

    #[test]
    fn diff_block_renders_path_header_and_hunks() {
        let block = TranscriptBlock::Diff {
            path: "src/x.rs".into(),
            hunks: vec![DiffHunk {
                lines: vec![DiffLine {
                    kind: DiffLineKind::Add,
                    text: "x".into(),
                }],
            }],
        };
        let lines = render_block(&block, &theme::DARK, 0);
        assert!(line_text(&lines[0]).contains("src/x.rs"));
        // body should have at least one Green span (diff adds are always green)
        let has_green = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green);
    }

    #[test]
    fn diff_block_with_no_hunks_renders_stub() {
        let block = TranscriptBlock::Diff {
            path: "src/x.rs".into(),
            hunks: vec![],
        };
        let lines = render_block(&block, &theme::DARK, 0);
        assert!(lines.iter().any(|l| line_text(l).contains("Updated")));
    }

    #[test]
    fn compacted_block_renders_with_summary() {
        let lines = render_block(
            &TranscriptBlock::Compacted {
                removed: 12,
                kept: 1,
            },
            &theme::DARK,
            0,
        );
        let s = line_text(&lines[0]);
        assert!(s.contains("12"));
        assert!(s.contains("compacted"));
    }
}

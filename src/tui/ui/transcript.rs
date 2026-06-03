//! Block-aware transcript renderer.
//!
//! Goal-144 replaces the single-line `StyledMessage::to_line()` ladder
//! with one render function per [`TranscriptBlock`] variant. A block
//! produces 1-or-more [`Line`]s; the chat panel concatenates the
//! results separated by a blank line.

use ratatui::prelude::*;

use crate::tui::app::{TranscriptBlock, UsageStats};
use crate::tui::ui::theme::Theme;
use crate::tui::ui::{diff, markdown};

/// Convert the entire transcript into a flat `Vec<Line>` with one
/// blank line between adjacent blocks. Folded ToolResult blocks
/// honour the `expanded` flag.
pub fn render_blocks(
    blocks: &[TranscriptBlock],
    _usage: &UsageStats,
    th: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.extend(render_block(b, th));
    }
    lines
}

/// Render a single block. Exposed for unit tests.
pub fn render_block(block: &TranscriptBlock, th: &Theme) -> Vec<Line<'static>> {
    match block {
        TranscriptBlock::User { text } => render_user(text, th),
        TranscriptBlock::Assistant {
            text,
            streaming,
            latency_ms,
        } => render_assistant(text, *streaming, *latency_ms, th),
        TranscriptBlock::ToolCall {
            name, args_preview, ..
        } => render_tool_call(name, args_preview, th),
        TranscriptBlock::ToolResult {
            name,
            success,
            output,
            expanded,
            ..
        } => render_tool_result(name, *success, output, *expanded, th),
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
    }
}

// ── User ──────────────────────────────────────────────────────────────

fn render_user(text: &str, th: &Theme) -> Vec<Line<'static>> {
    let gutter_style = Style::default().fg(th.system_bar);
    let mut out = vec![Line::from(vec![
        Span::styled("▎ ", Style::default().fg(th.user_bar)),
        Span::styled(
            "You".to_string(),
            Style::default()
                .fg(th.user_bar)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for line in text.lines() {
        out.push(Line::from(vec![
            Span::styled("│  ", gutter_style),
            Span::styled(line.to_string(), Style::default().fg(th.status_fg)),
        ]));
    }
    if text.is_empty() {
        out.push(Line::from(vec![Span::styled(
            "│  ".to_string(),
            gutter_style,
        )]));
    }
    out
}

// ── Assistant ─────────────────────────────────────────────────────────

fn render_assistant(
    text: &str,
    streaming: bool,
    latency_ms: Option<u64>,
    th: &Theme,
) -> Vec<Line<'static>> {
    let gutter_style = Style::default().fg(th.system_bar);
    let mut header = vec![
        Span::styled("▎ ", Style::default().fg(th.assistant_bar)),
        Span::styled(
            "Agent".to_string(),
            Style::default()
                .fg(th.assistant_bar)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(ms) = latency_ms {
        header.push(Span::raw("  "));
        header.push(Span::styled(
            format!("⏱ {:.1}s", ms as f64 / 1000.0),
            Style::default().fg(th.system_bar),
        ));
    }
    if streaming {
        header.push(Span::raw("  "));
        header.push(Span::styled(
            "…streaming".to_string(),
            Style::default()
                .fg(th.system_bar)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let mut out = vec![Line::from(header)];

    if text.is_empty() {
        out.push(Line::from(vec![Span::styled(
            "│  ".to_string(),
            gutter_style,
        )]));
        return out;
    }

    // Use the full pulldown-cmark based renderer for proper markdown parsing
    // (handles multi-paragraph blocks, nested lists, fenced code with syntax
    // highlighting, etc.).  We wrap each resulting line with the gutter prefix.
    let md_lines = markdown::render_markdown(text, 0);
    for md_line in md_lines {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(md_line.spans.len() + 1);
        spans.push(Span::styled("│  ", gutter_style));
        spans.extend(md_line.spans);
        out.push(Line::from(spans));
    }
    out
}

// ── ToolCall ──────────────────────────────────────────────────────────

fn render_tool_call(name: &str, args_preview: &str, th: &Theme) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("🔧", Style::default().fg(th.tool_call_icon)),
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(th.tool_call_icon)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            args_preview.to_string(),
            Style::default()
                .fg(th.system_bar)
                .add_modifier(Modifier::DIM),
        ),
    ])]
}

// ── ToolResult ────────────────────────────────────────────────────────

fn render_tool_result(
    name: &str,
    success: bool,
    output: &str,
    expanded: bool,
    th: &Theme,
) -> Vec<Line<'static>> {
    let (sigil, sigil_color) = if success {
        ("✓", th.tool_ok_fg)
    } else {
        ("✗", th.tool_err_fg)
    };
    let size = format_size(output.len());

    let mut out = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(sigil.to_string(), Style::default().fg(sigil_color)),
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(sigil_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(format!("({size})"), Style::default().fg(th.system_bar)),
    ])];

    let collected: Vec<&str> = output.lines().collect();
    let n = collected.len();
    let body_color = if success {
        th.status_fg
    } else {
        th.tool_err_fg
    };

    let visible: Vec<&&str> = if expanded || n <= 6 {
        collected.iter().collect()
    } else {
        collected.iter().take(3).collect()
    };
    for line in visible {
        out.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(th.system_bar)),
            Span::styled((*line).to_string(), Style::default().fg(body_color)),
        ]));
    }
    if !expanded && n > 6 {
        out.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(th.system_bar)),
            Span::styled(
                format!("… ({} more lines, press Ctrl+E to expand)", n - 3),
                Style::default()
                    .fg(th.system_bar)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
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
    use crate::tui::app::{DiffHunk, DiffLine, DiffLineKind, TranscriptBlock};
    use crate::tui::ui::theme;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn user_block_renders_label_and_body() {
        let lines = render_block(
            &TranscriptBlock::User {
                text: "hello world".into(),
            },
            &theme::DARK,
        );
        assert!(line_text(&lines[0]).contains("You"));
        assert!(line_text(&lines[1]).contains("hello world"));
    }

    #[test]
    fn assistant_block_includes_latency_when_set() {
        let lines = render_block(
            &TranscriptBlock::Assistant {
                text: "ok".into(),
                streaming: false,
                latency_ms: Some(1234),
            },
            &theme::DARK,
        );
        let header = line_text(&lines[0]);
        assert!(header.contains("Agent"));
        assert!(header.contains("⏱"));
        assert!(header.contains("1.2s"));
    }

    #[test]
    fn assistant_streaming_marker_present_when_streaming() {
        let lines = render_block(
            &TranscriptBlock::Assistant {
                text: "hel".into(),
                streaming: true,
                latency_ms: None,
            },
            &theme::DARK,
        );
        let header = line_text(&lines[0]);
        assert!(header.contains("streaming"));
    }

    #[test]
    fn tool_call_block_includes_name_and_preview() {
        let lines = render_block(
            &TranscriptBlock::ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                args_preview: "path=\"foo\"".into(),
            },
            &theme::DARK,
        );
        let s = line_text(&lines[0]);
        assert!(s.contains("read_file"));
        assert!(s.contains("path"));
    }

    #[test]
    fn tool_result_long_output_truncated_with_hint() {
        let output = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_block(
            &TranscriptBlock::ToolResult {
                id: "1".into(),
                name: "read_file".into(),
                success: true,
                output,
                expanded: false,
            },
            &theme::DARK,
        );
        // header + 3 lines + ellipsis = 5 lines
        assert_eq!(lines.len(), 5);
        let last = line_text(lines.last().unwrap());
        assert!(last.contains("Ctrl+E"));
        assert!(last.contains("more lines"));
    }

    #[test]
    fn tool_result_expanded_shows_all() {
        let output = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_block(
            &TranscriptBlock::ToolResult {
                id: "1".into(),
                name: "read_file".into(),
                success: true,
                output,
                expanded: true,
            },
            &theme::DARK,
        );
        // header + 10 body lines
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn tool_result_failure_uses_error_color() {
        let lines = render_block(
            &TranscriptBlock::ToolResult {
                id: "1".into(),
                name: "x".into(),
                success: false,
                output: "boom".into(),
                expanded: false,
            },
            &theme::DARK,
        );
        let header = &lines[0];
        // In the DARK theme, tool_err_fg = Color::Red
        let has_err_color = header
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme::DARK.tool_err_fg));
        assert!(has_err_color);
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
        let lines = render_block(&block, &theme::DARK);
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
        let lines = render_block(&block, &theme::DARK);
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
        );
        let s = line_text(&lines[0]);
        assert!(s.contains("12"));
        assert!(s.contains("compacted"));
    }
}

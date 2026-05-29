//! Block-aware transcript renderer.
//!
//! Goal-144 replaces the single-line `StyledMessage::to_line()` ladder
//! with one render function per [`TranscriptBlock`] variant. A block
//! produces 1-or-more [`Line`]s; the chat panel concatenates the
//! results separated by a blank line.

use ratatui::prelude::*;

use crate::app::{TranscriptBlock, UsageStats};
use crate::ui::diff;

/// Convert the entire transcript into a flat `Vec<Line>` with one
/// blank line between adjacent blocks. Folded ToolResult blocks
/// honour the `expanded` flag.
pub fn render_blocks(blocks: &[TranscriptBlock], _usage: &UsageStats) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.extend(render_block(b));
    }
    lines
}

/// Render a single block. Exposed for unit tests.
pub fn render_block(block: &TranscriptBlock) -> Vec<Line<'static>> {
    match block {
        TranscriptBlock::User { text } => render_user(text),
        TranscriptBlock::Assistant {
            text,
            streaming,
            latency_ms,
        } => render_assistant(text, *streaming, *latency_ms),
        TranscriptBlock::ToolCall {
            name, args_preview, ..
        } => render_tool_call(name, args_preview),
        TranscriptBlock::ToolResult {
            name,
            success,
            output,
            expanded,
            ..
        } => render_tool_result(name, *success, output, *expanded),
        TranscriptBlock::Diff { path, hunks } => render_diff(path, hunks),
        TranscriptBlock::Compacted { removed, kept } => render_compacted(*removed, *kept),
        TranscriptBlock::System { text } => render_system(text),
        TranscriptBlock::Error { text } => render_error(text),
    }
}

// ── User ──────────────────────────────────────────────────────────────

fn render_user(text: &str) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::styled("▎ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "You".to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    for line in text.lines() {
        out.push(Line::from(vec![
            Span::styled("│  ", Style::default().fg(Color::DarkGray)),
            Span::styled(line.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    if text.is_empty() {
        out.push(Line::from(vec![Span::styled(
            "│  ".to_string(),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    out
}

// ── Assistant ─────────────────────────────────────────────────────────

fn render_assistant(text: &str, streaming: bool, latency_ms: Option<u64>) -> Vec<Line<'static>> {
    let mut header = vec![
        Span::styled("▎ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Agent".to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(ms) = latency_ms {
        header.push(Span::raw("  "));
        header.push(Span::styled(
            format!("⏱ {:.1}s", ms as f64 / 1000.0),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if streaming {
        header.push(Span::raw("  "));
        header.push(Span::styled(
            "…streaming".to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let mut out = vec![Line::from(header)];
    for line in text.lines() {
        out.push(Line::from(vec![
            Span::styled("│  ", Style::default().fg(Color::DarkGray)),
            Span::styled(line.to_string(), Style::default().fg(Color::Cyan)),
        ]));
    }
    if text.is_empty() {
        out.push(Line::from(vec![Span::styled(
            "│  ".to_string(),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    out
}

// ── ToolCall ──────────────────────────────────────────────────────────

fn render_tool_call(name: &str, args_preview: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("  "),
        Span::styled("🔧", Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            args_preview.to_string(),
            Style::default()
                .fg(Color::DarkGray)
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
) -> Vec<Line<'static>> {
    let (sigil, sigil_color) = if success {
        ("✓", Color::Green)
    } else {
        ("✗", Color::Red)
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
        Span::styled(format!("({size})"), Style::default().fg(Color::DarkGray)),
    ])];

    let collected: Vec<&str> = output.lines().collect();
    let n = collected.len();
    let body_color = if success { Color::White } else { Color::Red };

    let visible: Vec<&&str> = if expanded || n <= 6 {
        collected.iter().collect()
    } else {
        collected.iter().take(3).collect()
    };
    for line in visible {
        out.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(Color::DarkGray)),
            Span::styled((*line).to_string(), Style::default().fg(body_color)),
        ]));
    }
    if !expanded && n > 6 {
        out.push(Line::from(vec![
            Span::styled("    │ ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("… ({} more lines, press Ctrl+E to expand)", n - 3),
                Style::default()
                    .fg(Color::DarkGray)
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

fn render_diff(path: &str, hunks: &[crate::app::DiffHunk]) -> Vec<Line<'static>> {
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
        Span::styled("⊕", Style::default().fg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!("Conversation compacted: {removed} messages → {kept} summary"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ])]
}

fn render_system(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        text.to_string(),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )])]
}

fn render_error(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        text.to_string(),
        Style::default().fg(Color::Red),
    )])]
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DiffHunk, DiffLine, DiffLineKind, TranscriptBlock};

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn user_block_renders_label_and_body() {
        let lines = render_block(&TranscriptBlock::User {
            text: "hello world".into(),
        });
        assert!(line_text(&lines[0]).contains("You"));
        assert!(line_text(&lines[1]).contains("hello world"));
    }

    #[test]
    fn assistant_block_includes_latency_when_set() {
        let lines = render_block(&TranscriptBlock::Assistant {
            text: "ok".into(),
            streaming: false,
            latency_ms: Some(1234),
        });
        let header = line_text(&lines[0]);
        assert!(header.contains("Agent"));
        assert!(header.contains("⏱"));
        assert!(header.contains("1.2s"));
    }

    #[test]
    fn assistant_streaming_marker_present_when_streaming() {
        let lines = render_block(&TranscriptBlock::Assistant {
            text: "hel".into(),
            streaming: true,
            latency_ms: None,
        });
        let header = line_text(&lines[0]);
        assert!(header.contains("streaming"));
    }

    #[test]
    fn tool_call_block_includes_name_and_preview() {
        let lines = render_block(&TranscriptBlock::ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            args_preview: "path=\"foo\"".into(),
        });
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
        let lines = render_block(&TranscriptBlock::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            success: true,
            output,
            expanded: false,
        });
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
        let lines = render_block(&TranscriptBlock::ToolResult {
            id: "1".into(),
            name: "read_file".into(),
            success: true,
            output,
            expanded: true,
        });
        // header + 10 body lines
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn tool_result_failure_uses_red_sigil() {
        let lines = render_block(&TranscriptBlock::ToolResult {
            id: "1".into(),
            name: "x".into(),
            success: false,
            output: "boom".into(),
            expanded: false,
        });
        let header = &lines[0];
        let has_red = header.spans.iter().any(|s| s.style.fg == Some(Color::Red));
        assert!(has_red);
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
        let lines = render_block(&block);
        assert!(line_text(&lines[0]).contains("src/x.rs"));
        // body should have at least one Green span
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
        let lines = render_block(&block);
        assert!(lines.iter().any(|l| line_text(l).contains("Updated")));
    }

    #[test]
    fn compacted_block_renders_with_summary() {
        let lines = render_block(&TranscriptBlock::Compacted {
            removed: 12,
            kept: 1,
        });
        let s = line_text(&lines[0]);
        assert!(s.contains("12"));
        assert!(s.contains("compacted"));
    }
}

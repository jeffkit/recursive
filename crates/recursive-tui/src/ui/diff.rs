//! Diff block renderer.
//!
//! Renders a [`TranscriptBlock::Diff`] as a header line (`📝 path`) plus
//! one line per [`DiffLine`] with `+` / `-` lines coloured green / red
//! and unchanged context lines greyed. When the diff has no hunks
//! (typically the synthesised "Created/Updated" stub for `write_file`)
//! we render a single muted line summarising the path.
//!
//! V4A patch parsing lives in [`crate::app::parse_v4a_patch`]; this
//! module is purely the visual layer.

use ratatui::prelude::*;

use crate::app::{DiffHunk, DiffLineKind};

/// Header line for a Diff block: `  📝 <path>`.
pub fn header_line(path: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled("📝", Style::default().fg(Color::Magenta)),
        Span::raw(" "),
        Span::styled(path.to_string(), Style::default().fg(Color::White)),
    ])
}

/// Render the body lines of a Diff block (no header).
pub fn body_lines(hunks: &[DiffHunk]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for hunk in hunks {
        for dl in &hunk.lines {
            let (sigil, color) = match dl.kind {
                DiffLineKind::Add => ("+", Color::Green),
                DiffLineKind::Remove => ("-", Color::Red),
                DiffLineKind::Context => (" ", Color::DarkGray),
            };
            out.push(Line::from(vec![
                Span::styled("    │ ".to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(sigil.to_string(), Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(dl.text.clone(), Style::default().fg(color)),
            ]));
        }
    }
    out
}

/// "No hunks" stub line used when a `write_file` only produced a
/// path summary.
pub fn empty_stub_line(path: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("    │ ".to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("Updated {path}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DiffLine;

    fn span_style_colors(line: &Line) -> Vec<Color> {
        line.spans.iter().filter_map(|s| s.style.fg).collect()
    }

    #[test]
    fn diff_renders_plus_minus_with_colors() {
        let hunks = vec![DiffHunk {
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Add,
                    text: "new line".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Remove,
                    text: "old line".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Context,
                    text: "ctx".into(),
                },
            ],
        }];
        let lines = body_lines(&hunks);
        assert_eq!(lines.len(), 3);
        assert!(span_style_colors(&lines[0]).contains(&Color::Green));
        assert!(span_style_colors(&lines[1]).contains(&Color::Red));
        // context line: at least one DarkGray span (the gutter)
        assert!(span_style_colors(&lines[2]).contains(&Color::DarkGray));
    }

    #[test]
    fn header_line_contains_path() {
        let line = header_line("src/agent.rs");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("src/agent.rs"));
        assert!(text.contains("📝"));
    }
}

//! Modal stack (Goal 146).
//!
//! A modal is a transient overlay drawn on top of the chat screen.
//! [`crate::tui::app::App`] owns a `Vec<Modal>`; the topmost element (the
//! "active" modal) is rendered centred over a half-screen window and
//! consumes all key events until popped.
//!
//! Modals never mutate runtime state directly — they are pure
//! read-only views over [`App`]. Side-effects (clear / exit /
//! compact) are routed through the command system in
//! [`crate::commands`] and the input dispatcher in
//! [`crate::tui::app::App`].

use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::app::{App, UsageStats};
use crate::tui::commands::CommandRegistry;

/// A simple read-only journal entry: filename + its first 30 lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    pub name: String,
    pub preview: String,
}

/// A confirmation request awaiting `y/n`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    Exit,
    Clear,
}

/// All modal flavours the TUI knows how to render.
///
/// Goal 146 ships Help / CostDetail / ModelInfo / ToolList / Journal
/// / Confirm. Goal 147 folds the Plan-mode confirmation into the
/// modal stack as `Modal::PlanReview`, replacing the dedicated
/// `AppScreen::PlanReview` (now removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    Help,
    CostDetail,
    ModelInfo,
    ToolList {
        entries: Vec<(String, String)>,
    },
    Journal {
        entries: Vec<JournalEntry>,
        selected: usize,
    },
    Confirm {
        prompt: String,
        on_yes: ConfirmAction,
    },
    /// Goal-147: structured plan-mode confirmation. `tool_calls`
    /// carries the pending tool calls as JSON values (see
    /// [`AgentEvent::PlanProposed`]). `edited_text` is reserved for
    /// future inline editing — Goal 147 only uses the `e` key to
    /// copy the plan into the prompt buffer and dismiss, so this
    /// field is currently always `None` but kept for forward
    /// compatibility per the goal's field schema.
    PlanReview {
        plan_text: String,
        tool_calls: Vec<serde_json::Value>,
        edited_text: Option<String>,
    },
}

impl Modal {
    /// Title shown in the modal's top border.
    pub fn title(&self) -> &'static str {
        match self {
            Modal::Help => " Help ",
            Modal::CostDetail => " Token usage ",
            Modal::ModelInfo => " Model ",
            Modal::ToolList { .. } => " Tools ",
            Modal::Journal { .. } => " Journal ",
            Modal::Confirm { .. } => " Confirm ",
            Modal::PlanReview { .. } => " Plan Proposal ",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the topmost modal centred on the frame area.
///
/// The dim backdrop is drawn first via [`Clear`]; the modal frame
/// occupies roughly two-thirds of the screen. The caller (chat
/// renderer) skips its own input cursor when a modal is active.
pub fn render(frame: &mut Frame, app: &App) {
    let Some(modal) = app.modals.last() else {
        return;
    };
    let area = centred_rect(frame.area(), 70, 70);

    // Dim backdrop.
    frame.render_widget(Clear, area);

    let body = match modal {
        Modal::Help => render_help_body(),
        Modal::CostDetail => render_cost_body(&app.usage, &app.model_name),
        Modal::ModelInfo => render_model_body(&app.model_name),
        Modal::ToolList { entries } => render_tool_body(entries),
        Modal::Journal { entries, selected } => render_journal_body(entries, *selected),
        Modal::Confirm { prompt, .. } => render_confirm_body(prompt),
        Modal::PlanReview {
            plan_text,
            tool_calls,
            ..
        } => render_plan_review(plan_text, tool_calls),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(modal.title())
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// Carve a centred rectangle out of `outer`, taking the requested
/// percentage of width and height.
fn centred_rect(outer: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = outer.width.saturating_mul(pct_w) / 100;
    let h = outer.height.saturating_mul(pct_h) / 100;
    Rect {
        x: outer.x + (outer.width.saturating_sub(w)) / 2,
        y: outer.y + (outer.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn render_help_body() -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);

    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        "Recursive TUI — Help".to_string(),
        header,
    )));
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled("Commands:".to_string(), header)));

    // Build the command list directly from the registry so it stays
    // in sync. We don't list aliases here to keep the table compact.
    let registry = CommandRegistry::default_set();
    for spec in registry.commands() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("/{:<10}", spec.name), key),
            Span::raw(" "),
            Span::raw(spec.summary.to_string()),
        ]));
    }

    out.push(Line::raw(""));
    out.push(Line::from(Span::styled("Keys:".to_string(), header)));
    let keys: &[(&str, &str)] = &[
        ("Enter", "Submit"),
        ("Shift+Enter", "Newline"),
        ("Shift+Tab", "Cycle input mode (prompt → bash → note)"),
        ("↑/↓ (empty)", "Browse history"),
        ("PgUp / PgDn", "Scroll transcript"),
        ("Ctrl+E", "Toggle expand on tool result / EOL in input"),
        ("Ctrl+A", "Move to line start"),
        ("Ctrl+C", "Interrupt (Step 5)"),
        ("Esc", "Close modal / cancel"),
        ("q (in modal)", "Close modal"),
    ];
    for (k, desc) in keys {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{k:<14}"), key),
            Span::raw(" "),
            Span::raw(desc.to_string()),
        ]));
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "Esc / q to close".to_string(),
        dim,
    )));
    out
}

fn render_cost_body(usage: &UsageStats, model: &str) -> Vec<Line<'static>> {
    let pricing = crate::tui::app::default_pricing_table();
    let cost_in = pricing
        .get(model)
        .map(|(rate, _)| (usage.total_input as f64) / 1000.0 * rate);
    let cost_out = pricing
        .get(model)
        .map(|(_, rate)| (usage.total_output as f64) / 1000.0 * rate);
    let cost_total = cost_in.zip(cost_out).map(|(a, b)| a + b);

    let fmt_cost = |c: Option<f64>| match c {
        Some(v) => format!("(${v:.4})"),
        None => String::from("(no pricing)"),
    };

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);

    let mut out = vec![Line::from(Span::styled(
        "Token usage (this session)".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("Input  : {:<7}  {}", usage.total_input, fmt_cost(cost_in)),
            body,
        ),
    ]));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("Output : {:<7}  {}", usage.total_output, fmt_cost(cost_out)),
            body,
        ),
    ]));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(
                "Total  : {:<7}  {}",
                usage.total_input.saturating_add(usage.total_output),
                fmt_cost(cost_total)
            ),
            body,
        ),
    ]));
    out.push(Line::raw(""));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(format!(
            "Last turn latency: {:.2} s",
            usage.last_latency_ms as f64 / 1000.0
        )),
    ]));
    out.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(format!("Provider         : {model}")),
    ]));
    out
}

fn render_model_body(model: &str) -> Vec<Line<'static>> {
    let api_base = std::env::var("RECURSIVE_API_BASE")
        .or_else(|_| std::env::var("OPENAI_API_BASE"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let provider = if model.starts_with("deepseek") {
        "deepseek"
    } else if model.starts_with("glm") {
        "zhipu"
    } else if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gpt") {
        "openai"
    } else {
        "unknown"
    };

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut out = vec![Line::from(Span::styled(
        "Current model".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    out.push(Line::from(format!("  Model    : {model}")));
    out.push(Line::from(format!("  Provider : {provider}")));
    out.push(Line::from(format!("  Endpoint : {api_base}")));
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "(read-only — switching models requires restart)".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    out
}

fn render_tool_body(entries: &[(String, String)]) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let mut out = vec![Line::from(Span::styled(
        format!("Available tools ({})", entries.len()),
        header,
    ))];
    out.push(Line::raw(""));
    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no tools registered)".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, desc) in entries {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{name:<16}"), key),
                Span::raw(" "),
                Span::raw(short_desc(desc, 60)),
            ]));
        }
    }
    out
}

fn render_journal_body(entries: &[JournalEntry], selected: usize) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut out = vec![Line::from(Span::styled(
        "Recent journal entries".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no entries in .dev/journal/)".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }
    for (i, entry) in entries.iter().enumerate() {
        let marker = if i == selected { "▶" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        out.push(Line::from(vec![
            Span::raw(format!(" {marker} ")),
            Span::styled(entry.name.clone(), style),
        ]));
    }
    out.push(Line::raw(""));
    if let Some(active) = entries.get(selected) {
        out.push(Line::from(Span::styled(
            format!("── {} ──", active.name),
            Style::default().fg(Color::DarkGray),
        )));
        for line in active.preview.lines() {
            out.push(Line::from(format!("  {line}")));
        }
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "↑/↓ navigate  |  Esc / q close".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    out
}

fn render_confirm_body(prompt: &str) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(prompt.to_string())];
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "  [y] yes   [n] no   [Esc] cancel".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    out
}

/// Goal-147: render the body of a [`Modal::PlanReview`].
///
/// Layout, mirroring the goal §1 ASCII sketch:
///
/// ```text
/// Plan Proposal
///
/// <plan_text multi-line, white>
///
/// Pending tools (N):
///   • name(arguments_preview)
///   • …
///
/// [y/Enter] Approve  [n/Esc] Reject  [e] Edit
/// ```
///
/// `tool_calls` is the JSON-shaped payload from
/// `AgentEvent::PlanProposed.tool_calls`. Each entry should have
/// `name` (string), optional `id` (string), and `arguments` (JSON).
/// We tolerate missing fields and render a best-effort preview.
pub fn render_plan_review(plan_text: &str, tool_calls: &[serde_json::Value]) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);

    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        "Plan Proposal".to_string(),
        header,
    )));
    out.push(Line::raw(""));
    for raw in plan_text.lines() {
        out.push(Line::from(Span::styled(raw.to_string(), body)));
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        format!("Pending tools ({}):", tool_calls.len()),
        header,
    )));
    if tool_calls.is_empty() {
        out.push(Line::from(Span::styled("  (none)".to_string(), dim)));
    } else {
        for tc in tool_calls {
            let name = tc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let args = tc
                .get("arguments")
                .map(plan_review_args_preview)
                .unwrap_or_default();
            out.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(name.to_string(), key),
                Span::styled(format!("({args})"), body),
            ]));
        }
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "[y/Enter] Approve  [n/Esc] Reject  [e] Edit".to_string(),
        dim,
    )));
    out
}

/// Format a single tool's `arguments` JSON value as a short preview
/// inside the PlanReview tool list. Strings get quoted-and-clamped,
/// objects get a `key=value` reduction (up to two keys), other JSON
/// values render via their `to_string()`.
fn plan_review_args_preview(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::String(s) => format!("\"{}\"", short(s, 40)),
        Value::Object(map) => {
            let mut parts = Vec::new();
            for (k, v) in map.iter().take(2) {
                let v_str = match v {
                    Value::String(s) => format!("\"{}\"", short(s, 24)),
                    other => short(&other.to_string(), 24),
                };
                parts.push(format!("{k}={v_str}"));
            }
            short(&parts.join(", "), 60)
        }
        Value::Null => String::new(),
        other => short(&other.to_string(), 60),
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn short_desc(desc: &str, max: usize) -> String {
    let one_line = desc.lines().next().unwrap_or("").trim();
    if one_line.chars().count() <= max {
        one_line.to_string()
    } else {
        let head: String = one_line.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Journal helpers
// ──────────────────────────────────────────────────────────────────────

/// Read up to `max_entries` `.md` files from `.dev/journal/`, sorted
/// by modification time descending. Each entry's preview is the
/// first 30 lines.
///
/// The path is hard-coded to `.dev/journal/` to keep `/journal` from
/// becoming an arbitrary file-read primitive (see goal §9 Notes).
pub fn load_recent_journal_entries(max_entries: usize) -> Vec<JournalEntry> {
    load_journal_from(std::path::Path::new(".dev/journal"), max_entries, 30)
}

/// Pure helper: read journal entries from `dir`. Exposed so tests can
/// point at a tempdir.
pub fn load_journal_from(
    dir: &std::path::Path,
    max_entries: usize,
    preview_lines: usize,
) -> Vec<JournalEntry> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let mut paths: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        paths.push((path, mtime));
    }
    paths.sort_by_key(|b| std::cmp::Reverse(b.1));
    paths.truncate(max_entries);

    paths
        .into_iter()
        .map(|(path, _)| {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let preview = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .take(preview_lines)
                .collect::<Vec<_>>()
                .join("\n");
            JournalEntry { name, preview }
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;

    #[test]
    fn esc_pops_top_modal() {
        let mut app = App::new();
        app.modals.push(Modal::Help);
        app.modals.push(Modal::CostDetail);
        assert_eq!(app.modals.len(), 2);
        // Simulating an Esc when a modal is active calls into App's
        // modal-priority path; here we directly assert pop.
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.modals.len(), 1);
        assert_eq!(app.modals.last(), Some(&Modal::Help));
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.modals.is_empty());
    }

    #[test]
    fn confirm_yes_executes_action_and_pops() {
        let mut app = App::new();
        app.modals.push(Modal::Confirm {
            prompt: "Quit?".into(),
            on_yes: ConfirmAction::Exit,
        });
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.modals.is_empty());
        assert!(app.should_quit);
    }

    #[test]
    fn confirm_n_pops_without_action() {
        let mut app = App::new();
        app.modals.push(Modal::Confirm {
            prompt: "Clear?".into(),
            on_yes: ConfirmAction::Clear,
        });
        let blocks_before = app.blocks.len();
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('n'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.modals.is_empty());
        assert_eq!(app.blocks.len(), blocks_before);
    }

    #[test]
    fn journal_up_down_moves_selection() {
        let mut app = App::new();
        app.modals.push(Modal::Journal {
            entries: vec![
                JournalEntry {
                    name: "a.md".into(),
                    preview: "p1".into(),
                },
                JournalEntry {
                    name: "b.md".into(),
                    preview: "p2".into(),
                },
                JournalEntry {
                    name: "c.md".into(),
                    preview: "p3".into(),
                },
            ],
            selected: 0,
        });
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
        match app.modals.last() {
            Some(Modal::Journal { selected, .. }) => assert_eq!(*selected, 1),
            other => panic!("expected Journal, got {other:?}"),
        }
        app.handle_modal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        ));
        match app.modals.last() {
            Some(Modal::Journal { selected, .. }) => assert_eq!(*selected, 0),
            other => panic!("expected Journal, got {other:?}"),
        }
    }

    #[test]
    fn journal_loader_picks_recent_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("old.md"),
            "line1\nline2\nline3\nline4\nline5\nline6",
        )
        .unwrap();
        // Sleep enough so the second file's mtime is strictly newer
        // — filesystems may collapse same-second mtimes.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.join("new.md"), "fresh content\nmore").unwrap();
        std::fs::write(dir.join("ignored.txt"), "should be skipped").unwrap();

        let entries = load_journal_from(dir, 5, 3);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "new.md");
        assert!(entries[0].preview.starts_with("fresh content"));
        assert_eq!(entries[1].name, "old.md");
        // 30-line cap enforced (we asked for 3): old.md preview has 3 lines.
        assert_eq!(entries[1].preview.lines().count(), 3);
    }

    #[test]
    fn render_help_lists_registered_commands() {
        let lines = render_help_body();
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("/help"));
        assert!(text.contains("/clear"));
        assert!(text.contains("/exit"));
        // Keys section
        assert!(text.contains("Shift+Tab"));
        assert!(text.contains("Ctrl+E"));
    }
}

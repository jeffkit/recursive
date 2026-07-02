//! Modal stack (Goal 146).
//!
//! A modal is a transient overlay drawn on top of the chat screen.
//! [`crate::app::App`] owns a `Vec<Modal>`; the topmost element (the
//! "active" modal) is rendered as a full-width left-accent panel (inspired
//! by Claude Code's `/mcp` panel style) that covers the conversation area
//! and consumes all key events until popped.
//!
//! Modals never mutate runtime state directly — they are pure
//! read-only views over [`App`]. Side-effects (clear / exit /
//! compact) are routed through the command system in
//! [`crate::commands`] and the input dispatcher in
//! [`crate::app::App`].

use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, UsageStats};
use crate::commands::CommandRegistry;

/// A simple read-only journal entry: filename + its first 30 lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    pub name: String,
    pub preview: String,
}

// ── Goal-171: resume-picker entry ────────────────────────────────────────────

/// One entry in the [`Modal::ResumePicker`] list.
#[derive(Clone, Debug, PartialEq)]
pub struct ResumeEntry {
    /// Absolute path to the session directory.
    pub session_dir: std::path::PathBuf,
    /// Short description (≤40 chars): first user prompt or goal text.
    pub slug: String,
    /// Human-readable last-updated date ("2026-06-01 14:22").
    pub updated_at: String,
    /// Number of recorded messages.
    pub turn_count: usize,
    /// Cumulative cost in USD (0.0 if unknown).
    pub cost_usd: f64,
}

/// One entry in the [`Modal::McpServers`] list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEntry {
    pub name: String,
    pub transport: String,
    pub enabled: bool,
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
#[derive(Clone, Debug, PartialEq)]
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
    /// Goal-171: session resume picker. Shows a list of recent sessions
    /// ordered by last-updated; ↑/↓ selects, Enter resumes, Esc cancels.
    ResumePicker {
        entries: Vec<ResumeEntry>,
        selected: usize,
    },
    /// Goal-173: MCP server list. Shows configured MCP servers with
    /// their transport type and enabled status.
    McpServers {
        entries: Vec<McpEntry>,
        selected: usize,
    },
    /// Goal-230: skill-hub installation flow. Three-stage interactive modal:
    /// Results (search results) → Files (zip contents) → Preview (file viewer).
    #[cfg(feature = "skill-hub")]
    SkillInstall(crate::ui::modal::SkillInstallState),
}

// ── Goal-230: SkillInstall modal state ───────────────────────────────────────

/// Which sub-page the SkillInstall modal is on.
#[cfg(feature = "skill-hub")]
#[derive(Clone, Debug, PartialEq)]
pub enum SkillInstallPage {
    /// Showing search results; `selected` is the highlighted row index.
    Results { selected: usize },
    /// Showing the file tree for the chosen skill; `selected` is the
    /// highlighted file index.
    Files { selected: usize },
    /// Showing the content of file at index `file_idx`, scrolled to `scroll`.
    Preview { file_idx: usize, scroll: u16 },
}

/// Cloneable display state for the `SkillInstall` modal.
#[cfg(feature = "skill-hub")]
#[derive(Clone, Debug, PartialEq)]
pub struct SkillInstallState {
    pub query: String,
    pub results: Vec<crate::events::SkillSearchResult>,
    /// Populated after the user selects a result and the tool downloads the zip.
    pub slug: Option<String>,
    pub files: Vec<crate::events::SkillZipFile>,
    pub page: SkillInstallPage,
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
            Modal::ResumePicker { .. } => " Resume Session ",
            Modal::McpServers { .. } => " MCP Servers ",
            #[cfg(feature = "skill-hub")]
            Modal::SkillInstall(_) => " Install Skill ",
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────

/// Render the topmost modal as a full-width left-accent panel.
///
/// The panel covers the conversation area (full width, top-anchored)
/// and leaves the status bar + input rows visible below. The clear
/// backdrop is applied only to the panel area, so the persistent
/// status / input chrome beneath remains usable.
pub fn render(frame: &mut Frame, app: &App) {
    let Some(modal) = app.modals.last() else {
        return;
    };
    // Full-width panel: cover the whole frame width, leave the bottom
    // rows for the status bar and input box.
    let area = panel_rect(frame.area());

    // Clear the background behind the panel.
    frame.render_widget(Clear, area);

    let body = match modal {
        Modal::Help => render_help_body(&app.commands),
        Modal::CostDetail => render_cost_body(&app.usage, &app.model_name),
        Modal::ModelInfo => render_model_body(&app.model_name),
        Modal::ToolList { entries } => render_tool_body(entries),
        Modal::Journal { entries, selected } => render_journal_body(entries, *selected),
        Modal::Confirm { prompt, .. } => render_confirm_body(prompt),
        Modal::ResumePicker { entries, selected } => render_resume_picker_body(entries, *selected),
        Modal::McpServers { entries, selected } => render_mcp_servers_body(entries, *selected),
        Modal::PlanReview {
            plan_text,
            tool_calls,
            ..
        } => render_plan_review(plan_text, tool_calls),
        #[cfg(feature = "skill-hub")]
        Modal::SkillInstall(state) => render_skill_install(state),
    };

    // Left-accent panel style (Claude Code /mcp inspired):
    // - single LEFT border as a cyan accent bar — no enclosing box
    // - title embedded in the body content, not in the border
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.modal_scroll, 0));
    frame.render_widget(para, area);
}

/// Build the panel rect: full terminal width, top-anchored, reserving
/// the bottom rows for the persistent status bar and input widget.
fn panel_rect(outer: Rect) -> Rect {
    // Reserve 4 rows: 1 for the status bar and ~3 for the input box.
    // `saturating_sub` keeps the height ≥ 0 for very small terminals.
    const BOTTOM_RESERVED: u16 = 4;
    Rect {
        x: outer.x,
        y: outer.y,
        width: outer.width,
        height: outer.height.saturating_sub(BOTTOM_RESERVED),
    }
}

fn render_help_body(registry: &CommandRegistry) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);
    let skill_style = Style::default().fg(Color::Green);

    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        "Recursive TUI — Help".to_string(),
        header,
    )));
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled("Commands:".to_string(), header)));

    // Built-in commands from the registry.
    for spec in registry.commands() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("/{:<10}", spec.name), key),
            Span::raw(" "),
            Span::raw(spec.summary.to_string()),
        ]));
    }

    // Goal-169: skill-backed commands loaded from .recursive/skills/.
    let skills = registry.skill_commands();
    if !skills.is_empty() {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled(
            "Skill Commands:".to_string(),
            header,
        )));
        for skill in skills {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("/{:<10}", skill.name), skill_style),
                Span::raw(" "),
                Span::raw(skill.description.clone()),
            ]));
        }
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
    let pricing = recursive::llm::pricing_for(model);
    let cost_in = pricing.map(|p| (usage.total_input as f64) * p.input_per_million / 1_000_000.0);
    let cost_out =
        pricing.map(|p| (usage.total_output as f64) * p.output_per_million / 1_000_000.0);
    let cost_total = cost_in.zip(cost_out).map(|(a, b)| a + b);

    let fmt_cost = |c: Option<f64>| match c {
        Some(v) => format!("(${v:.4})"),
        None => String::from("(no pricing)"),
    };

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

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
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "  Esc / q to close".to_string(),
        dim,
    )));
    out
}

fn render_model_body(model: &str) -> Vec<Line<'static>> {
    // Pull from Config so the modal shows the same endpoint the runtime
    // will use, including the `provider.preset` chain. Previously this
    // read RECURSIVE_API_BASE directly and used a `model.starts_with(...)`
    // heuristic — both bypassed preset resolution and displayed wrong
    // values when the user set `provider.preset = "deepseek"`.
    let cfg = recursive::config::Config::from_env().ok();
    let api_base = cfg
        .as_ref()
        .map(|c| c.api_base.clone())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let provider = cfg
        .as_ref()
        .and_then(|c| c.preset.clone())
        .or_else(|| {
            recursive::providers::find_preset_by_api_base(&api_base).map(|p| p.id.to_string())
        })
        .unwrap_or_else(|| "custom".to_string());

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
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
        "  (read-only — switching models requires restart)".to_string(),
        dim,
    )));
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "  Esc / q to close".to_string(),
        dim,
    )));
    out
}

fn render_tool_body(entries: &[(String, String)]) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);
    let mut out = vec![Line::from(Span::styled(
        format!("Available tools ({})", entries.len()),
        header,
    ))];
    out.push(Line::raw(""));
    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no tools registered)".to_string(),
            dim,
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
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "  Esc / q to close".to_string(),
        dim,
    )));
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
        // Limit preview to 12 lines; use modal_scroll (↑/↓) to read more.
        for line in active.preview.lines().take(12) {
            out.push(Line::from(format!("  {line}")));
        }
        let total = active.preview.lines().count();
        if total > 12 {
            out.push(Line::from(Span::styled(
                format!("  … ({} more lines, ↑/↓ to scroll)", total - 12),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "↑/↓ navigate  |  Esc / q close".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    out
}

fn render_resume_picker_body(entries: &[ResumeEntry], selected: usize) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let mut out = vec![Line::from(Span::styled(
        "Recent sessions  (↑/↓ select · Enter resume · Esc cancel)".to_string(),
        header,
    ))];
    out.push(Line::raw(""));

    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no saved sessions found)".to_string(),
            dim,
        )));
        return out;
    }

    for (i, entry) in entries.iter().enumerate() {
        let sel_marker = if i == selected { "▶" } else { " " };
        let row_style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let line_text = format!(
            " {} {:<42} turns:{:>3}  {}",
            sel_marker, entry.slug, entry.turn_count, entry.updated_at
        );
        out.push(Line::from(Span::styled(line_text, row_style)));
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "↑/↓ navigate  |  Enter resume  |  Esc cancel".to_string(),
        dim,
    )));
    out
}

fn render_mcp_servers_body(entries: &[McpEntry], selected: usize) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let green = Style::default().fg(Color::Green);
    let disabled = Style::default().fg(Color::DarkGray);

    let mut out = vec![Line::from(Span::styled(
        "Configured MCP servers  (↑/↓ navigate · Esc close)".to_string(),
        header,
    ))];
    out.push(Line::raw(""));

    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  No MCP servers configured".to_string(),
            dim,
        )));
        return out;
    }

    for (i, entry) in entries.iter().enumerate() {
        let sel_marker = if i == selected { "▶" } else { " " };
        let bullet = if entry.enabled { "●" } else { "○" };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if entry.enabled {
            green
        } else {
            disabled
        };
        let line_text = format!(
            " {} {}  {}  ({})",
            sel_marker, bullet, entry.name, entry.transport
        );
        out.push(Line::from(Span::styled(line_text, style)));
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "↑/↓ navigate  |  Esc / q close".to_string(),
        dim,
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
    out.push(Line::from(vec![
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
// Session helpers (Goal-171)
// ──────────────────────────────────────────────────────────────────────

/// Load up to `limit` recent sessions from `workspace`, sorted by
/// `updated_at` descending. Silently skips dirs with missing/corrupt metadata.
pub fn load_recent_sessions(workspace: &std::path::Path, limit: usize) -> Vec<ResumeEntry> {
    let pairs =
        match recursive::session::SessionReader::list_sessions_sorted_by_updated_at(workspace) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

    pairs
        .into_iter()
        .take(limit)
        .map(|(dir, meta)| {
            let raw_slug = if !meta.goal.is_empty() {
                meta.goal.clone()
            } else {
                meta.first_prompt.clone().unwrap_or_default()
            };
            let slug = if raw_slug.chars().count() > 40 {
                let s: String = raw_slug.chars().take(39).collect();
                format!("{s}…")
            } else {
                raw_slug
            };
            let updated_at = meta
                .updated_at
                .get(..16)
                .unwrap_or(&meta.updated_at)
                .replace('T', " ");
            // SessionCost tracks token counts only; no USD field.
            let cost_usd = 0.0_f64;
            let _ = meta.cost; // suppress unused warning
            ResumeEntry {
                session_dir: dir,
                slug,
                updated_at,
                turn_count: meta.message_count as usize,
                cost_usd,
            }
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Goal-230: skill-hub install modal renderer
// (placed before the test module so clippy's items_after_test_module
// lint is not triggered)
// ──────────────────────────────────────────────────────────────────────

#[cfg(feature = "skill-hub")]
fn render_skill_install(state: &SkillInstallState) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);

    let mut out: Vec<Line<'static>> = Vec::new();

    match &state.page {
        SkillInstallPage::Results { selected: sel } => {
            out.push(Line::from(Span::styled(
                format!(" Install Skill — \"{}\" ", state.query),
                header,
            )));
            out.push(Line::raw(""));

            if state.results.is_empty() {
                out.push(Line::from(Span::styled(
                    "  No results found. Try a different query.",
                    dim,
                )));
            } else {
                for (i, r) in state.results.iter().enumerate() {
                    let stars = format!("⭐ {:>2}", r.stars);
                    let downloads = if r.downloads >= 1_000 {
                        format!("↓ {:.1}k", r.downloads as f64 / 1000.0)
                    } else {
                        format!("↓ {}", r.downloads)
                    };
                    let version = format!("v{}", r.version);
                    let row_text = format!(
                        " {:<24} {:>6}  {:>8}  {:>8} ",
                        r.name, stars, downloads, version
                    );
                    if i == *sel {
                        out.push(Line::from(Span::styled(row_text, selected_style)));
                        let desc = if r.description.chars().count() > 68 {
                            let s: String = r.description.chars().take(67).collect();
                            format!("  {}…", s)
                        } else {
                            format!("  {}", r.description)
                        };
                        out.push(Line::from(Span::styled(desc, dim)));
                    } else {
                        out.push(Line::from(Span::raw(row_text)));
                    }
                }
            }
            out.push(Line::raw(""));
            out.push(Line::from(vec![
                Span::styled(" ↑↓ ", key),
                Span::raw("navigate  "),
                Span::styled(" Enter ", key),
                Span::raw("select & browse files  "),
                Span::styled(" Esc ", key),
                Span::raw("cancel"),
            ]));
        }

        SkillInstallPage::Files { selected: sel } => {
            let slug = state.slug.as_deref().unwrap_or("?");
            out.push(Line::from(Span::styled(
                format!(" {slug} — Files "),
                header,
            )));
            out.push(Line::raw(""));
            if state.files.is_empty() {
                out.push(Line::from(Span::styled("  (no files)", dim)));
            } else {
                for (i, f) in state.files.iter().enumerate() {
                    let size_str = if f.size >= 1024 {
                        format!("{:.1}kb", f.size as f64 / 1024.0)
                    } else {
                        format!("{} b", f.size)
                    };
                    let row_text = format!(" {:<45} {:>8} ", f.path, size_str);
                    if i == *sel {
                        out.push(Line::from(Span::styled(row_text, selected_style)));
                    } else {
                        out.push(Line::from(Span::raw(row_text)));
                    }
                }
            }
            out.push(Line::raw(""));
            out.push(Line::from(vec![
                Span::styled(" ↑↓ ", key),
                Span::raw("navigate  "),
                Span::styled(" v ", key),
                Span::raw("preview file  "),
                Span::styled(" y ", key),
                Span::raw("confirm install  "),
                Span::styled(" Esc ", key),
                Span::raw("cancel"),
            ]));
        }

        SkillInstallPage::Preview { file_idx, .. } => {
            let file = state.files.get(*file_idx);
            let fname = file.map(|f| f.path.as_str()).unwrap_or("?");
            out.push(Line::from(Span::styled(format!(" {fname} "), header)));
            out.push(Line::raw(""));
            if let Some(f) = file {
                for line in f.content.lines() {
                    out.push(Line::from(Span::raw(line.to_string())));
                }
            }
            out.push(Line::raw(""));
            out.push(Line::from(vec![
                Span::styled(" PgUp/PgDn ", key),
                Span::raw("scroll  "),
                Span::styled(" Esc ", key),
                Span::raw("back to files"),
            ]));
        }
    }

    out
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

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
        let registry = CommandRegistry::default_set();
        let lines = render_help_body(&registry);
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

    #[test]
    fn mcp_entry_renders_enabled() {
        let entry = McpEntry {
            name: "my-server".to_string(),
            transport: "stdio".to_string(),
            enabled: true,
        };
        let lines = render_mcp_servers_body(&[entry], 0);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("my-server"));
        assert!(text.contains("stdio"));
        assert!(text.contains("●"));
    }

    #[test]
    fn render_help_lists_skill_commands_when_present() {
        use crate::skill_commands::SkillCommand;
        use std::path::PathBuf;
        let skill = SkillCommand {
            name: "my-skill".to_string(),
            description: "A test skill".to_string(),
            aliases: Vec::new(),
            argument_hint: String::new(),
            allowed_tools: None,
            prompt_template: "Do $ARGUMENTS".to_string(),
            source_path: PathBuf::from("my-skill.md"),
        };
        let registry = CommandRegistry::default_set().with_skill_commands(vec![skill]);
        let lines = render_help_body(&registry);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Skill Commands:"));
        assert!(text.contains("/my-skill"));
        assert!(text.contains("A test skill"));
    }

    #[test]
    fn load_recent_journal_entries_returns_repo_entries() {
        // kills load_recent_journal_entries -> vec![] (742:5). Build a
        // tempdir with a `.dev/journal/*.md` file, cd into it, and confirm
        // orig returns a non-empty list (the mutant returns empty).
        let tmp = tempfile::tempdir().expect("tempdir");
        let journal_dir = tmp.path().join(".dev").join("journal");
        std::fs::create_dir_all(&journal_dir).expect("mkdir");
        std::fs::write(journal_dir.join("entry.md"), "# entry\nbody\n").expect("write");
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(tmp.path()).expect("cd");
        let entries = load_recent_journal_entries(5);
        std::env::set_current_dir(prev).expect("restore cwd");
        assert!(
            !entries.is_empty(),
            "expected recent journal entries from .dev/journal"
        );
    }

    #[test]
    fn modal_title_returns_label_per_variant() {
        // kills Modal::title -> ""/"xyzzy" (145:9).
        assert_eq!(Modal::Help.title(), " Help ");
        assert_eq!(Modal::CostDetail.title(), " Token usage ");
        assert_eq!(Modal::ModelInfo.title(), " Model ");
    }

    #[test]
    fn short_returns_input_under_max_and_truncates_over() {
        // kills short -> String::new()/"xyzzy" (713:5) and `<=`->`>` (713:26).
        assert_eq!(short("abc", 3), "abc"); // boundary: 3 <= 3 -> no truncate
        assert_eq!(short("abcdef", 4), "abc…");
    }

    #[test]
    fn short_desc_takes_first_line_trimmed() {
        // kills short_desc -> String::new()/"xyzzy" (722:5) and `<=`->`>` (723:33).
        assert_eq!(short_desc("line1\nline2", 5), "line1"); // boundary 5<=5
        assert_eq!(short_desc("  trim  ", 10), "trim");
    }

    #[test]
    fn plan_review_args_preview_quotes_string_value() {
        // kills plan_review_args_preview -> String::new()/"xyzzy" (693:5).
        assert_eq!(
            plan_review_args_preview(&serde_json::json!("abc")),
            "\"abc\""
        );
    }
}

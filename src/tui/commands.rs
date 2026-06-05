//! Slash-command registry and the core built-in commands (Goal 146).
//!
//! A [`CommandSpec`] is a static description (name, aliases, summary,
//! handler). The [`CommandRegistry`] wraps a `Vec<CommandSpec>` (built-ins)
//! and a `Vec<SkillCommand>` (Goal-169 skill-backed dynamic commands), and
//! provides exact lookup (with alias resolution) and prefix search
//! for the completion menu.
//!
//! Handlers are split into [`CommandHandler::Sync`] (mutate
//! [`AppState`] directly and return a [`CommandOutcome`]) and
//! [`CommandHandler::Async`] (push [`UserAction`]s for the backend
//! worker to service).
//!
//! Side-effects: handlers may push transcript blocks, modify
//! `App.modals`, or set `App.should_quit`. They never block the
//! event loop.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::app::{App as AppState, CommandPanelState};
use crate::tui::events::UserAction;
use crate::tui::skill_commands::SkillCommand;
use crate::tui::ui::modal::Modal;

/// One registered slash command.
#[derive(Clone)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    pub usage: &'static str,
    pub handler: CommandHandler,
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("summary", &self.summary)
            .field("usage", &self.usage)
            .field("handler", &"<fn>")
            .finish()
    }
}

/// What a command's handler does when invoked.
#[derive(Clone)]
pub enum CommandHandler {
    /// Synchronous: mutate AppState in place. Returns a
    /// [`CommandOutcome`] describing the next UI action (push modal,
    /// surface error, …).
    Sync(fn(&mut AppState, &[String]) -> CommandOutcome),
    /// Asynchronous: returns a list of [`UserAction`]s to dispatch to
    /// the backend worker. The handler may also mutate AppState
    /// (e.g. push a System block confirming the request).
    Async(fn(&mut AppState, &[String]) -> Vec<UserAction>),
}

/// Result of a synchronous command.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Handler completed; nothing more for the dispatcher to do.
    Done,
    /// Push an error block describing why the command failed.
    Error(String),
    /// Push a modal onto the stack.
    OpenModal(Modal),
    /// Open an interactive panel below the input box. The panel owns
    /// key events until the user confirms (Enter) or cancels (Esc).
    OpenPanel(crate::tui::app::CommandPanelState),
}

/// Vec-backed slash-command registry.
///
/// Stores built-in [`CommandSpec`] entries (static) plus Goal-169
/// skill-backed commands loaded from `.recursive/skills/`.
#[derive(Clone, Debug)]
pub struct CommandRegistry {
    commands: Vec<CommandSpec>,
    /// Goal-169: skill-backed dynamic commands.
    skill_commands: Vec<SkillCommand>,
}

impl CommandRegistry {
    /// Build the default registry — the built-in commands.
    pub fn default_set() -> Self {
        Self {
            commands: vec![
                CommandSpec {
                    name: "help",
                    aliases: &["?"],
                    summary: "Show commands & key bindings",
                    usage: "/help",
                    handler: CommandHandler::Sync(cmd_help),
                },
                CommandSpec {
                    name: "clear",
                    aliases: &["cls"],
                    summary: "Clear conversation transcript",
                    usage: "/clear",
                    handler: CommandHandler::Sync(cmd_clear),
                },
                CommandSpec {
                    name: "compact",
                    aliases: &[],
                    summary: "Compact the transcript",
                    usage: "/compact",
                    handler: CommandHandler::Async(cmd_compact),
                },
                CommandSpec {
                    name: "cost",
                    aliases: &[],
                    summary: "Show token & cost detail",
                    usage: "/cost",
                    handler: CommandHandler::Sync(cmd_cost),
                },
                CommandSpec {
                    name: "model",
                    aliases: &[],
                    summary: "Show current model",
                    usage: "/model",
                    handler: CommandHandler::Sync(cmd_model),
                },
                CommandSpec {
                    name: "status",
                    aliases: &[],
                    summary: "Print runtime status",
                    usage: "/status",
                    handler: CommandHandler::Sync(cmd_status),
                },
                CommandSpec {
                    name: "tools",
                    aliases: &[],
                    summary: "List available tools",
                    usage: "/tools",
                    handler: CommandHandler::Sync(cmd_tools),
                },
                CommandSpec {
                    name: "plan",
                    aliases: &[],
                    summary: "Toggle planning mode (/plan on|off)",
                    usage: "/plan on|off",
                    handler: CommandHandler::Async(cmd_plan),
                },
                CommandSpec {
                    name: "journal",
                    aliases: &[],
                    summary: "Show recent .dev/journal entries",
                    usage: "/journal",
                    handler: CommandHandler::Sync(cmd_journal),
                },
                CommandSpec {
                    name: "permissions",
                    aliases: &["perm"],
                    summary: "Toggle runtime permission hook (/permissions on|off)",
                    usage: "/permissions on|off",
                    handler: CommandHandler::Sync(cmd_permissions),
                },
                CommandSpec {
                    name: "exit",
                    aliases: &["quit", "q"],
                    summary: "Quit the TUI",
                    usage: "/exit",
                    handler: CommandHandler::Sync(cmd_exit),
                },
                // Goal-168: condition-based autonomous loop.
                CommandSpec {
                    name: "goal",
                    aliases: &[],
                    summary: "Autonomous loop until condition met",
                    usage: "/goal <cond> [or stop after N turns] | /goal | /goal clear",
                    handler: CommandHandler::Async(cmd_goal),
                },
                // Goal-171: session resume picker.
                CommandSpec {
                    name: "resume",
                    aliases: &["r"],
                    summary: "Pick a previous conversation to continue",
                    usage: "/resume",
                    handler: CommandHandler::Sync(cmd_resume),
                },
                // Goal-173: MCP server list.
                CommandSpec {
                    name: "mcp",
                    aliases: &[],
                    summary: "List configured MCP servers",
                    usage: "/mcp",
                    handler: CommandHandler::Async(cmd_mcp),
                },
                // Goal-174: theme picker.
                CommandSpec {
                    name: "theme",
                    aliases: &[],
                    summary: "Switch colour theme (dark / light / solarized)",
                    usage: "/theme <name>",
                    handler: CommandHandler::Sync(cmd_theme),
                },
            ],
            skill_commands: Vec::new(),
        }
    }

    /// Goal-169: register skill-backed commands alongside built-ins.
    ///
    /// Skill commands appear in lookup and search results.  A skill command
    /// whose name collides with a built-in is silently shadowed by the
    /// built-in (built-ins win).
    pub fn with_skill_commands(mut self, skills: Vec<SkillCommand>) -> Self {
        self.skill_commands = skills;
        self
    }

    /// Return a reference to the loaded skill commands.
    pub fn skill_commands(&self) -> &[SkillCommand] {
        &self.skill_commands
    }

    /// Read-only access to the registered commands. Used by the help
    /// modal and by tests.
    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }

    /// Look up a built-in command by canonical name *or* alias. The leading
    /// `/` is **not** part of `name` — strip it before calling.
    pub fn lookup(&self, name: &str) -> Option<&CommandSpec> {
        self.commands
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))
    }

    /// Look up a skill command by canonical name or alias.
    ///
    /// Built-ins shadow skill commands: if a built-in with the same name
    /// exists, this returns `None` (callers should check `lookup` first).
    pub fn lookup_skill(&self, name: &str) -> Option<&SkillCommand> {
        // Don't expose skill if a built-in has the same name.
        if self.lookup(name).is_some() {
            return None;
        }
        self.skill_commands
            .iter()
            .find(|s| s.name == name || s.aliases.iter().any(|a| a == name))
    }

    /// Prefix-match across canonical names and aliases for **built-in** commands.
    /// Returns commands whose name (or any alias) starts with `prefix`,
    /// sorted alphabetically by canonical name. An empty prefix
    /// returns *all* commands.
    pub fn search(&self, prefix: &str) -> Vec<&CommandSpec> {
        let prefix = prefix.trim_start_matches('/');
        let mut hits: Vec<&CommandSpec> = self
            .commands
            .iter()
            .filter(|c| {
                c.name.starts_with(prefix) || c.aliases.iter().any(|a| a.starts_with(prefix))
            })
            .collect();
        hits.sort_by_key(|c| c.name);
        hits
    }

    /// Prefix-match across all skill commands.
    pub fn search_skills(&self, prefix: &str) -> Vec<&SkillCommand> {
        let prefix = prefix.trim_start_matches('/');
        let mut hits: Vec<&SkillCommand> = self
            .skill_commands
            .iter()
            .filter(|s| {
                s.name.starts_with(prefix) || s.aliases.iter().any(|a| a.starts_with(prefix))
            })
            .collect();
        hits.sort_by_key(|s| s.name.as_str());
        hits
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::default_set()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Handler implementations
// ──────────────────────────────────────────────────────────────────────

fn cmd_help(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let registry = app.commands.clone();
    let lines = build_help_lines(&registry);
    let count = lines.len();
    CommandOutcome::OpenPanel(
        CommandPanelState::new("help", lines)
            .with_item_count(count)
            .with_hint("↑↓ / PgUp/PgDn scroll  ·  esc close"),
    )
}

fn cmd_clear(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    app.reset_transcript();
    CommandOutcome::Done
}

fn cmd_compact(app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    app.push_system("Compacting transcript…");
    vec![UserAction::Compact]
}

fn cmd_cost(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let lines = build_cost_lines(&app.usage, &app.model_name);
    CommandOutcome::OpenPanel(CommandPanelState::new("cost", lines).with_hint("esc close"))
}

fn cmd_model(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let lines = build_model_lines();
    CommandOutcome::OpenPanel(CommandPanelState::new("model", lines).with_hint("esc close"))
}

fn cmd_status(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let uptime_secs = app.start_time.elapsed().as_secs();
    let total_tokens = app.usage.total_input.saturating_add(app.usage.total_output);
    let text = format!(
        "Status — turn {}, blocks {}, tokens {}, uptime {}s",
        app.turn_count,
        app.blocks.len(),
        total_tokens,
        uptime_secs,
    );
    app.push_system(text);
    CommandOutcome::Done
}

fn cmd_tools(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let lines = build_tool_lines(&app.tool_catalog);
    CommandOutcome::OpenPanel(
        CommandPanelState::new("tools", lines).with_hint("↑↓ / PgUp/PgDn scroll  ·  esc close"),
    )
}

fn cmd_plan(app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    app.push_system(
        "PlanFirst mode has been removed. Use the agent's plan-mode tools \
         (enter_plan_mode / exit_plan_mode) for human-in-the-loop planning."
            .to_string(),
    );
    Vec::new()
}

fn cmd_journal(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let entries = crate::tui::ui::modal::load_recent_journal_entries(5);
    let item_count = entries.len();
    let lines = build_journal_lines(&entries, 0);
    let ctx = serde_journal_context(&entries);
    CommandOutcome::OpenPanel(
        CommandPanelState::new("journal", lines)
            .with_selection(0)
            .with_item_count(item_count)
            .with_hint("↑↓ select entry  ·  esc close")
            .with_context(ctx),
    )
}

fn cmd_exit(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    app.should_quit = true;
    CommandOutcome::Done
}

fn cmd_permissions(app: &mut AppState, args: &[String]) -> CommandOutcome {
    let arg = args.first().map(|s| s.to_lowercase());
    let on = match arg.as_deref() {
        Some("on") | Some("true") | Some("1") => true,
        Some("off") | Some("false") | Some("0") => false,
        _ => {
            let current = if app
                .permission_hook_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                "on"
            } else {
                "off"
            };
            app.push_error(format!("Usage: /permissions on|off  (currently {current})"));
            return CommandOutcome::Done;
        }
    };
    app.permission_hook_enabled
        .store(on, std::sync::atomic::Ordering::Relaxed);
    if !on {
        // Clear auto-allow list when disabling so it starts fresh next time.
        app.auto_allowed_tools.clear();
        // If a modal is open, deny and close it.
        if let Some(old) = app.pending_permission.take() {
            let _ = old.reply.send(false);
        }
    }
    app.push_system(format!(
        "Permissions hook: {}",
        if on { "on" } else { "off" }
    ));
    CommandOutcome::Done
}

/// `/goal [<condition> [or stop after N turns]] | clear`
///
/// - `/goal <cond>` → start a condition-based autonomous loop.
/// - `/goal <cond> or stop after N turns` → same with explicit max turns.
/// - `/goal` (no args) → show current goal status.
/// - `/goal clear` → clear the active goal immediately.
fn cmd_goal(app: &mut AppState, args: &[String]) -> Vec<UserAction> {
    if args.is_empty() {
        // Show current status.
        let status = app
            .active_goal
            .as_ref()
            .map(|g| {
                format!(
                    "Goal: \"{}\" — turn {}/{} — {}",
                    g.condition,
                    g.turns,
                    g.max_turns,
                    g.last_reason.as_deref().unwrap_or("pursuing")
                )
            })
            .unwrap_or_else(|| "No active goal.".to_string());
        app.push_system(status);
        return Vec::new();
    }

    if args.len() == 1 && args[0].eq_ignore_ascii_case("clear") {
        app.active_goal = None;
        app.push_system("Goal cleared.");
        return vec![UserAction::ClearGoal];
    }

    // Parse: "<condition> [or stop after N turns]"
    let raw = args.join(" ");
    let (condition, max_turns) = parse_goal_args(&raw);
    app.push_system(format!("Goal set: \"{condition}\" (max {max_turns} turns)"));
    vec![UserAction::SetGoal {
        condition,
        max_turns,
    }]
}

/// Parse `"<condition> [or stop after N turns]"` from the raw argument string.
/// Returns `(condition, max_turns)`. Default max_turns = 20.
fn parse_goal_args(raw: &str) -> (String, u32) {
    // Look for " or stop after N turns" suffix (case-insensitive).
    let lower = raw.to_lowercase();
    if let Some(pos) = lower.rfind(" or stop after ") {
        let suffix = &raw[pos + " or stop after ".len()..];
        // Try to parse the first token as a number.
        let n: u32 = suffix
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let condition = raw[..pos].trim().to_string();
        return (condition, n);
    }
    (raw.trim().to_string(), 20)
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

fn cmd_resume(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    use crate::tui::ui::modal::load_recent_sessions;
    let workspace = app.workspace_path.clone();
    let entries = load_recent_sessions(&workspace, 20);
    if entries.is_empty() {
        return CommandOutcome::Error("No saved sessions found.".into());
    }
    let item_count = entries.len();
    let lines = build_resume_lines(&entries, 0);
    let ctx = serde_resume_context(&entries);
    CommandOutcome::OpenPanel(
        CommandPanelState::new("resume", lines)
            .with_selection(0)
            .with_item_count(item_count)
            .with_hint("↑↓ select  ·  enter resume  ·  esc cancel")
            .with_context(ctx),
    )
}

fn cmd_mcp(_app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    vec![UserAction::ListMcpServers]
}

fn cmd_theme(app: &mut AppState, args: &[String]) -> CommandOutcome {
    use crate::tui::ui::theme::ALL_THEMES;
    if args.is_empty() {
        // Open the interactive theme picker panel.
        let current = app.theme.name;
        let item_count = ALL_THEMES.len();
        let selected = ALL_THEMES
            .iter()
            .position(|t| t.name == current)
            .unwrap_or(0);
        let lines = build_theme_picker_lines(current, selected);
        let ctx = ALL_THEMES
            .iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join("\n");
        return CommandOutcome::OpenPanel(
            CommandPanelState::new("theme", lines)
                .with_selection(selected)
                .with_item_count(item_count)
                .with_hint("↑↓ select  ·  enter apply  ·  esc cancel")
                .with_context(ctx),
        );
    }
    let requested = args[0].to_lowercase();
    let found = crate::tui::ui::theme::find_theme(&requested);
    if found.name == requested {
        app.theme = found;
        app.push_system(format!("Theme switched to '{}'.", found.name));
    } else {
        let theme_list: Vec<&str> = ALL_THEMES.iter().map(|t| t.name).collect();
        let names = theme_list.join(", ");
        app.push_error(format!(
            "Unknown theme '{}'. Available: {}",
            requested, names
        ));
    }
    CommandOutcome::Done
}

// ──────────────────────────────────────────────────────────────────────
// Line builder helpers (called by command handlers above)
// ──────────────────────────────────────────────────────────────────────

pub fn build_help_lines(registry: &crate::tui::commands::CommandRegistry) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let skill_style = Style::default().fg(Color::Green);

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        "Recursive TUI — Help".to_string(),
        header,
    )));
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled("Commands:".to_string(), header)));
    for spec in registry.commands() {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("/{:<10}", spec.name), key),
            Span::raw(" "),
            Span::raw(spec.summary.to_string()),
        ]));
    }
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
        ("Shift+Tab", "Cycle input mode"),
        ("↑/↓ (empty)", "Browse history"),
        ("PgUp / PgDn", "Scroll transcript"),
        ("Ctrl+E", "Toggle expand on tool result / EOL"),
        ("Ctrl+A", "Move to line start"),
        ("Ctrl+C", "Interrupt (double-press to exit)"),
        ("Esc", "Close panel / clear input"),
    ];
    for (k, desc) in keys {
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{k:<14}"), key),
            Span::raw(" "),
            Span::raw(desc.to_string()),
        ]));
    }
    out
}

pub fn build_cost_lines(usage: &crate::tui::app::UsageStats, model: &str) -> Vec<Line<'static>> {
    let pricing = crate::llm::pricing_for(model);
    let cost_in = pricing.map(|p| (usage.total_input as f64) * p.input_per_million / 1_000_000.0);
    let cost_out =
        pricing.map(|p| (usage.total_output as f64) * p.output_per_million / 1_000_000.0);
    let cost_total = cost_in.zip(cost_out).map(|(a, b)| a + b);
    let fmt_cost = |c: Option<f64>| match c {
        Some(v) => format!("(${v:.4})"),
        None => "(no pricing)".to_string(),
    };
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);

    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
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

pub fn build_model_lines() -> Vec<Line<'static>> {
    let cfg = crate::config::Config::from_env().ok();
    let api_base = cfg
        .as_ref()
        .map(|c| c.api_base.clone())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let provider = cfg
        .as_ref()
        .and_then(|c| c.preset.clone())
        .or_else(|| crate::providers::find_preset_by_api_base(&api_base).map(|p| p.id.to_string()))
        .unwrap_or_else(|| "custom".to_string());
    let model = cfg
        .as_ref()
        .map(|c| c.model.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
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
    out
}

pub fn build_tool_lines(entries: &[(String, String)]) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key = Style::default().fg(Color::Cyan);
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
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
            let one_line = desc.lines().next().unwrap_or("").trim();
            let short = if one_line.chars().count() > 60 {
                let head: String = one_line.chars().take(59).collect();
                format!("{head}…")
            } else {
                one_line.to_string()
            };
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{name:<16}"), key),
                Span::raw(" "),
                Span::raw(short),
            ]));
        }
    }
    out
}

pub fn build_journal_lines(
    entries: &[crate::tui::ui::modal::JournalEntry],
    selected: usize,
) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
        "Recent journal entries".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    if entries.is_empty() {
        out.push(Line::from(Span::styled(
            "  (no entries in .dev/journal/)".to_string(),
            dim,
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
            dim,
        )));
        for line in active.preview.lines().take(12) {
            out.push(Line::from(format!("  {line}")));
        }
        let total = active.preview.lines().count();
        if total > 12 {
            out.push(Line::from(Span::styled(
                format!("  … ({} more lines)", total - 12),
                dim,
            )));
        }
    }
    out
}

/// Serialise journal entry names into the panel context so
/// `handle_command_panel_key` can reload them on selection change.
pub fn serde_journal_context(entries: &[crate::tui::ui::modal::JournalEntry]) -> String {
    entries
        .iter()
        .map(|e| e.name.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_resume_lines(
    entries: &[crate::tui::ui::modal::ResumeEntry],
    selected: usize,
) -> Vec<Line<'static>> {
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
        "Recent sessions".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    for (i, entry) in entries.iter().enumerate() {
        let marker = if i == selected { "▶" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let text = format!(
            " {} {:<42} turns:{:>3}  {}",
            marker, entry.slug, entry.turn_count, entry.updated_at
        );
        out.push(Line::from(Span::styled(text, style)));
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        "↑/↓ navigate  ·  Enter resume  ·  Esc cancel".to_string(),
        dim,
    )));
    out
}

/// Serialise session_dir paths (one per line) into the context so
/// `handle_command_panel_key` can reconstruct `UserAction::ResumeSession`.
pub fn serde_resume_context(entries: &[crate::tui::ui::modal::ResumeEntry]) -> String {
    entries
        .iter()
        .map(|e| e.session_dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_theme_picker_lines(current: &str, selected: usize) -> Vec<Line<'static>> {
    use crate::tui::ui::theme::ALL_THEMES;
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
        format!("Choose theme  (current: {current})"),
        header,
    ))];
    out.push(Line::raw(""));
    for (i, theme) in ALL_THEMES.iter().enumerate() {
        let marker = if i == selected { "▶" } else { " " };
        let style = if i == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        out.push(Line::from(Span::styled(
            format!(" {} {}", marker, theme.name),
            style,
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, TranscriptBlock};

    fn invoke(app: &mut App, line: &str) -> InvokeResult {
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap_or("");
        let args: Vec<String> = parts.map(String::from).collect();
        let registry = app.commands.clone();
        let Some(spec) = registry.lookup(name) else {
            app.push_error(format!("Unknown command: /{name}. Try /help."));
            return InvokeResult::Unknown;
        };
        match &spec.handler {
            CommandHandler::Sync(f) => InvokeResult::Sync(f(app, &args)),
            CommandHandler::Async(f) => InvokeResult::Async(f(app, &args)),
        }
    }

    #[derive(Debug)]
    enum InvokeResult {
        Sync(CommandOutcome),
        Async(Vec<UserAction>),
        Unknown,
    }

    #[test]
    fn cmd_mcp_is_registered() {
        let r = CommandRegistry::default_set();
        assert!(r.lookup("mcp").is_some(), "/mcp should be registered");
        let spec = r.lookup("mcp").unwrap();
        assert!(matches!(spec.handler, CommandHandler::Async(_)));
    }

    // Goal-174: theme command tests
    #[test]
    fn cmd_theme_switches_to_light() {
        let mut app = App::new();
        assert_eq!(app.theme.name, "dark");
        invoke(&mut app, "theme light");
        assert_eq!(app.theme.name, "light");
    }

    #[test]
    fn cmd_theme_no_args_opens_picker_panel() {
        let mut app = App::new();
        let r = invoke(&mut app, "theme");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => {
                assert_eq!(panel.command_name, "theme");
                assert!(panel.selected.is_some());
            }
            other => panic!("expected OpenPanel for /theme, got {other:?}"),
        }
    }

    #[test]
    fn cmd_theme_unknown_shows_error() {
        let mut app = App::new();
        invoke(&mut app, "theme neon");
        // Theme unchanged (still dark) because neon isn't known.
        assert_eq!(app.theme.name, "dark");
    }

    #[test]
    fn registry_finds_help_by_name_and_alias() {
        let r = CommandRegistry::default_set();
        assert!(r.lookup("help").is_some());
        assert!(r.lookup("?").is_some());
        // Fully-qualified `/help` shouldn't match — caller strips the
        // slash before lookup.
        assert!(r.lookup("/help").is_none());
        assert!(r.lookup("nope").is_none());
    }

    #[test]
    fn registry_includes_all_fifteen_commands() {
        let r = CommandRegistry::default_set();
        let names: Vec<&str> = r.commands().iter().map(|c| c.name).collect();
        for expected in &[
            "help",
            "clear",
            "compact",
            "cost",
            "model",
            "status",
            "tools",
            "plan",
            "journal",
            "exit",
            "permissions",
            "goal",
            "mcp",
            "theme",
        ] {
            assert!(
                names.contains(expected),
                "missing /{expected}: have {names:?}"
            );
        }
        // 11 built-in commands plus /goal, /resume, /mcp, and /theme = 15.
        assert_eq!(names.len(), 15);
    }

    #[test]
    fn registry_search_returns_prefix_matches_sorted() {
        let r = CommandRegistry::default_set();
        // "c" prefix matches clear, compact, cost.
        let hits: Vec<&str> = r.search("c").iter().map(|c| c.name).collect();
        assert_eq!(hits, vec!["clear", "compact", "cost"]);
        // alias-prefix hit: "?" matches /help via alias.
        let hits: Vec<&str> = r.search("?").iter().map(|c| c.name).collect();
        assert!(hits.contains(&"help"));
        // Empty prefix returns everything (sorted).
        let hits: Vec<&str> = r.search("").iter().map(|c| c.name).collect();
        assert_eq!(hits.len(), 15);
        // Sorted check.
        let mut sorted = hits.clone();
        sorted.sort();
        assert_eq!(hits, sorted);
    }

    #[test]
    fn clear_resets_transcript_and_usage() {
        let mut app = App::new();
        app.usage.total_input = 1000;
        app.usage.total_output = 500;
        app.blocks.push(TranscriptBlock::User {
            text: "hello".into(),
        });
        app.blocks.push(TranscriptBlock::Assistant {
            text: "hi".into(),
            streaming: false,
            latency_ms: None,
        });
        let _ = invoke(&mut app, "clear");
        // Reset clears all old blocks and pushes the cleared message.
        assert_eq!(app.blocks.len(), 1);
        assert!(matches!(
            &app.blocks[0],
            TranscriptBlock::System { text } if text.contains("cleared")
        ));
        assert_eq!(app.usage.total_input, 0);
        assert_eq!(app.usage.total_output, 0);
    }

    #[test]
    fn exit_sets_should_quit() {
        let mut app = App::new();
        let _ = invoke(&mut app, "exit");
        assert!(app.should_quit);
        let mut app2 = App::new();
        let _ = invoke(&mut app2, "q");
        assert!(app2.should_quit);
        let mut app3 = App::new();
        let _ = invoke(&mut app3, "quit");
        assert!(app3.should_quit);
    }

    #[test]
    fn status_appends_system_block_with_turn_count() {
        let mut app = App::new();
        app.turn_count = 7;
        let _ = invoke(&mut app, "status");
        let last = app.blocks.last().unwrap();
        match last {
            TranscriptBlock::System { text } => {
                assert!(text.contains("turn 7"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_pushes_error_block() {
        let mut app = App::new();
        let r = invoke(&mut app, "frobnicate");
        assert!(matches!(r, InvokeResult::Unknown));
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => {
                assert!(text.contains("Unknown command"), "got {text:?}");
            }
            other => panic!("expected Error block, got {other:?}"),
        }
    }

    #[test]
    fn plan_command_shows_deprecation_notice() {
        let mut app = App::new();
        let r = invoke(&mut app, "plan on");
        match r {
            InvokeResult::Async(actions) => {
                assert!(actions.is_empty(), "expected no actions after plan removal");
            }
            other => panic!("expected async result, got {other:?}"),
        }
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("PlanFirst mode has been removed"))
            }
            other => panic!("expected System block with deprecation notice, got {other:?}"),
        }
    }

    #[test]
    fn help_opens_panel() {
        let mut app = App::new();
        let r = invoke(&mut app, "help");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => {
                assert_eq!(panel.command_name, "help");
                let text: String = panel
                    .lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
                    .collect();
                assert!(text.contains("/help"));
            }
            other => panic!("expected OpenPanel for help, got {other:?}"),
        }
    }

    #[test]
    fn cost_opens_panel() {
        let mut app = App::new();
        let r = invoke(&mut app, "cost");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => {
                assert_eq!(panel.command_name, "cost");
            }
            other => panic!("expected OpenPanel for cost, got {other:?}"),
        }
    }

    #[test]
    fn tools_opens_panel_with_catalog() {
        let mut app = App::new();
        let r = invoke(&mut app, "tools");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => {
                assert_eq!(panel.command_name, "tools");
                let text: String = panel
                    .lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
                    .collect();
                assert!(text.contains("tools"));
            }
            other => panic!("expected OpenPanel for tools, got {other:?}"),
        }
    }

    #[test]
    fn compact_returns_compact_action() {
        let mut app = App::new();
        let r = invoke(&mut app, "compact");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions, vec![UserAction::Compact]);
            }
            other => panic!("expected Async([Compact]), got {other:?}"),
        }
    }
}

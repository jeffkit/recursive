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

use crate::app::{App as AppState, CommandPanelState};
use crate::events::UserAction;
use crate::skill_commands::SkillCommand;
use crate::ui::modal::Modal;

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
    OpenPanel(crate::app::CommandPanelState),
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
                    name: "add-dir",
                    aliases: &["adddir"],
                    summary: "Grant the agent access to an extra directory",
                    usage: "/add-dir <path> [--ro]",
                    handler: CommandHandler::Sync(cmd_add_dir),
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
                // Goal-323: event-driven loop.
                CommandSpec {
                    name: "loop",
                    aliases: &[],
                    summary: "Event-driven loop (/loop start|stop|trigger)",
                    usage: "/loop start <goal> [max N] | /loop stop | /loop trigger <text>",
                    handler: CommandHandler::Async(cmd_loop),
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

    /// Goal-322: replace skill commands in-place (for lazy reload).
    pub fn set_skill_commands(&mut self, skills: Vec<SkillCommand>) {
        self.skill_commands = skills;
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
    let entries = crate::ui::modal::load_recent_journal_entries(5);
    let item_count = entries.len();
    let lines = build_journal_lines(&entries, 0);
    let ctx = serde_journal_context(&entries);
    CommandOutcome::OpenPanel(
        CommandPanelState::new("journal", lines)
            .with_selection(0)
            .with_item_count(item_count)
            .with_list_offset(2)
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

/// `/add-dir <path> [--ro]`
///
/// Grant the agent runtime access to a directory outside the workspace by
/// appending it to the session-mutable sandbox roots. `--ro` (or `:ro`
/// suffix on the path) makes the grant read-only. Existing roots are
/// de-duplicated; re-adding a known path just reports it.
fn cmd_add_dir(app: &mut AppState, args: &[String]) -> CommandOutcome {
    if args.is_empty() {
        let listed = app
            .session_roots
            .read()
            .map(|roots| {
                if roots.is_empty() {
                    "No extra directories granted this session.".to_string()
                } else {
                    roots
                        .iter()
                        .map(|(p, t)| {
                            format!(
                                "- {} ({})",
                                p.display(),
                                match t {
                                    recursive::tools::AccessTier::ReadOnly => "ro",
                                    recursive::tools::AccessTier::ReadWrite => "rw",
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
            .unwrap_or_else(|_| "No extra directories granted this session.".to_string());
        app.push_system(format!(
            "Usage: /add-dir <path> [--ro]\n\nGranted this session:\n{listed}"
        ));
        return CommandOutcome::Done;
    }

    let read_only = args.iter().any(|a| a == "--ro" || a == "-r");
    let raw = args.iter().find(|a| !a.starts_with('-'));
    let Some(raw) = raw else {
        app.push_error("Usage: /add-dir <path> [--ro]");
        return CommandOutcome::Done;
    };
    // Allow a `:ro` suffix as shorthand for read-only.
    let (raw_path, ro_suffix) = raw
        .strip_suffix(":ro")
        .map(|p| (p, true))
        .unwrap_or((raw.as_str(), false));
    let read_only = read_only || ro_suffix;

    let candidate = std::path::Path::new(raw_path);
    let canonical = match candidate.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            app.push_error(format!(
                "/add-dir: cannot resolve \"{}\": {}",
                candidate.display(),
                e
            ));
            return CommandOutcome::Done;
        }
    };
    if !canonical.is_dir() {
        app.push_error(format!(
            "/add-dir: \"{}\" is not a directory",
            canonical.display()
        ));
        return CommandOutcome::Done;
    }

    let tier = if read_only {
        recursive::tools::AccessTier::ReadOnly
    } else {
        recursive::tools::AccessTier::ReadWrite
    };

    let already = app
        .session_roots
        .read()
        .map(|roots| roots.iter().any(|(p, _)| *p == canonical))
        .unwrap_or(false);
    if already {
        app.push_system(format!("Already granted: {}", canonical.display()));
        return CommandOutcome::Done;
    }

    if let Ok(mut roots) = app.session_roots.write() {
        roots.push((canonical.clone(), tier));
    }
    app.push_system(format!(
        "Granted agent access to {} ({})",
        canonical.display(),
        if read_only { "read-only" } else { "read-write" }
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

/// `/loop [start <goal> [max N]] | stop | trigger <text>`
fn cmd_loop(app: &mut AppState, args: &[String]) -> Vec<UserAction> {
    if args.is_empty() {
        // Show current loop status.
        let status = app
            .loop_state
            .as_ref()
            .map(|ls| {
                format!(
                    "Loop active — goal: \"{}\", turns: {}/{}",
                    ls.goal,
                    ls.turns_run,
                    if ls.max_turns > 0 {
                        ls.max_turns.to_string()
                    } else {
                        "unlimited".to_string()
                    }
                )
            })
            .unwrap_or_else(|| "No active loop.".to_string());
        app.push_system(status);
        return Vec::new();
    }

    let sub = args[0].as_str();
    match sub {
        "start" => {
            // Parse: /loop start <goal> [max N]
            let raw = args[1..].join(" ");
            if raw.trim().is_empty() {
                app.push_error("Usage: /loop start <goal> [max N]");
                return Vec::new();
            }
            let (goal, max_turns) = parse_loop_start_args(&raw);
            app.push_system(format!(
                "Loop started: \"{}\" (max {} turns)",
                goal,
                if max_turns > 0 {
                    max_turns.to_string()
                } else {
                    "unlimited".to_string()
                }
            ));
            app.loop_state = Some(crate::app::LoopUiState {
                goal: goal.clone(),
                turns_run: 0,
                max_turns,
            });
            vec![UserAction::StartLoop { goal, max_turns }]
        }
        "stop" => {
            app.loop_state = None;
            app.push_system("Loop stopped.");
            vec![UserAction::StopLoop]
        }
        "trigger" => {
            let text = args[1..].join(" ");
            if text.trim().is_empty() {
                app.push_error("Usage: /loop trigger <text>");
                return Vec::new();
            }
            app.push_system(format!("Loop trigger: {text}"));
            vec![UserAction::LoopTrigger {
                source: "manual".to_string(),
                prompt: text,
            }]
        }
        _ => {
            app.push_error(format!(
                "Unknown /loop sub-command: {sub}. Try /loop start|stop|trigger."
            ));
            Vec::new()
        }
    }
}

/// Parse `"<goal> [max N]"` from the raw argument string.
/// Returns `(goal, max_turns)`. Default max_turns = 0 (unlimited).
fn parse_loop_start_args(raw: &str) -> (String, u32) {
    let lower = raw.to_lowercase();
    if let Some(pos) = lower.rfind(" max ") {
        let suffix = &raw[pos + 5..];
        let n: u32 = suffix
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let goal = raw[..pos].trim().to_string();
        return (goal, n);
    }
    (raw.trim().to_string(), 0)
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
    use crate::ui::modal::load_recent_sessions;
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
            .with_list_offset(2)
            .with_hint("↑↓ select  ·  enter resume  ·  esc cancel")
            .with_context(ctx),
    )
}

fn cmd_mcp(_app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    vec![UserAction::ListMcpServers]
}

fn cmd_theme(app: &mut AppState, args: &[String]) -> CommandOutcome {
    use crate::ui::theme::ALL_THEMES;
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
                .with_list_offset(2)
                .with_hint("↑↓ select  ·  enter apply  ·  esc cancel")
                .with_context(ctx),
        );
    }
    let requested = args[0].to_lowercase();
    let found = crate::ui::theme::find_theme(&requested);
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

pub fn build_help_lines(registry: &crate::commands::CommandRegistry) -> Vec<Line<'static>> {
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

pub fn build_cost_lines(usage: &crate::app::UsageStats, model: &str) -> Vec<Line<'static>> {
    let pricing = recursive::llm::pricing_for(model);
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
    let model = cfg
        .as_ref()
        .map(|c| c.model.clone())
        .unwrap_or_else(|| "unknown".to_string());
    // `Config::from_env` already resolves the api_key through the full
    // chain (env → file → preset's key_env). An empty/None key here means
    // no provider is usable — the runtime would build as Offline. Surface
    // that explicitly so `/model` doesn't show the hardcoded
    // `deepseek-v4-flash` fallback as if it were a live configuration.
    let configured = cfg
        .as_ref()
        .and_then(|c| c.api_key.as_deref())
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let warn = Style::default().fg(Color::Red);
    let mut out: Vec<Line<'static>> = vec![Line::from(Span::styled(
        "Current model".to_string(),
        header,
    ))];
    out.push(Line::raw(""));
    if !configured {
        out.push(Line::from(Span::styled(
            "  Not configured — no API key set. Run `recursive init` \
             outside the TUI, then restart."
                .to_string(),
            warn,
        )));
        out.push(Line::raw(""));
    }
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
    entries: &[crate::ui::modal::JournalEntry],
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
pub fn serde_journal_context(entries: &[crate::ui::modal::JournalEntry]) -> String {
    entries
        .iter()
        .map(|e| e.name.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_resume_lines(
    entries: &[crate::ui::modal::ResumeEntry],
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
pub fn serde_resume_context(entries: &[crate::ui::modal::ResumeEntry]) -> String {
    entries
        .iter()
        .map(|e| e.session_dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_theme_picker_lines(current: &str, selected: usize) -> Vec<Line<'static>> {
    use crate::ui::theme::ALL_THEMES;
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
    use crate::app::{App, TranscriptBlock};

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

    #[test]
    fn cmd_add_dir_is_registered() {
        let r = CommandRegistry::default_set();
        assert!(
            r.lookup("add-dir").is_some(),
            "/add-dir should be registered"
        );
        assert!(r.lookup("adddir").is_some(), "/adddir alias should resolve");
    }

    #[test]
    fn cmd_add_dir_grants_readwrite_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        // Fresh session_roots slot from App::new() starts empty.
        assert!(app
            .session_roots
            .read()
            .map(|r| r.is_empty())
            .unwrap_or(true));
        invoke(&mut app, &format!("add-dir {}", tmp.path().display()));
        let roots = app
            .session_roots
            .read()
            .expect("read session_roots")
            .clone();
        assert_eq!(roots.len(), 1, "exactly one root granted: {roots:?}");
        assert_eq!(roots[0].0, tmp.path().canonicalize().unwrap());
        assert!(matches!(
            roots[0].1,
            recursive::tools::AccessTier::ReadWrite
        ));
    }

    #[test]
    fn cmd_add_dir_ro_suffix_makes_readonly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        invoke(&mut app, &format!("add-dir {}:ro", tmp.path().display()));
        let roots = app
            .session_roots
            .read()
            .expect("read session_roots")
            .clone();
        assert_eq!(roots.len(), 1);
        assert!(matches!(roots[0].1, recursive::tools::AccessTier::ReadOnly));
    }

    #[test]
    fn cmd_add_dir_dedupes_known_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().display().to_string();
        let mut app = App::new();
        invoke(&mut app, &format!("add-dir {path}"));
        invoke(&mut app, &format!("add-dir {path}"));
        let len = app.session_roots.read().map(|r| r.len()).unwrap_or(0);
        assert_eq!(len, 1, "re-adding a known path must not duplicate");
    }

    #[test]
    fn cmd_add_dir_rejects_missing_path() {
        let mut app = App::new();
        invoke(&mut app, "add-dir /this/path/does/not/exist/recursive-test");
        let len = app.session_roots.read().map(|r| r.len()).unwrap_or(0);
        assert_eq!(len, 0, "missing path must not be granted");
        // An error block should have been pushed.
        let has_error = app
            .blocks
            .iter()
            .any(|b| matches!(b, TranscriptBlock::Error { text } if text.contains("add-dir")));
        assert!(has_error, "expected an error block mentioning /add-dir");
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
    fn theme_panel_list_offset_aligns_highlight_with_marker() {
        // Regression: the highlight bar (indexed via list_offset + selected)
        // must land on the same row as the `▶` marker that the line builder
        // draws. Both must point at the same `lines` index.
        let mut app = App::new();
        let r = invoke(&mut app, "theme");
        let panel = match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => panel,
            other => panic!("expected OpenPanel for /theme, got {other:?}"),
        };
        let sel = panel.selected.expect("theme panel has a selection");
        let highlight_idx = sel + panel.list_offset;
        // The builder draws `▶` on the selected item's row.
        let marker_idx = panel
            .lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains('▶')))
            .expect("a ▶ marker should be present");
        assert_eq!(
            highlight_idx, marker_idx,
            "highlight bar row must match the ▶ marker row"
        );
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
    fn registry_includes_all_builtin_commands() {
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
            "add-dir",
            "goal",
            "mcp",
            "theme",
        ] {
            assert!(
                names.contains(expected),
                "missing /{expected}: have {names:?}"
            );
        }
        // 15 named above plus one lazily-registered built-in (/resume) = 16.
        // Plus /loop (Goal-323) = 17.
        assert_eq!(names.len(), 17);
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
        assert_eq!(hits.len(), 17);
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

    // ── Goal-323: /loop command tests ───────────────────────────────

    #[test]
    fn parse_loop_start_args_no_max_defaults_unlimited() {
        let (goal, max) = parse_loop_start_args("watch the build");
        assert_eq!(goal, "watch the build");
        assert_eq!(max, 0);
    }

    #[test]
    fn parse_loop_start_args_with_max_n() {
        let (goal, max) = parse_loop_start_args("watch the build max 5");
        assert_eq!(goal, "watch the build");
        assert_eq!(max, 5);
    }

    #[test]
    fn parse_loop_start_args_max_case_insensitive() {
        let (goal, max) = parse_loop_start_args("GO MAX 3");
        assert_eq!(goal, "GO");
        assert_eq!(max, 3);
    }

    #[test]
    fn parse_loop_start_args_max_zero_is_explicit() {
        let (goal, max) = parse_loop_start_args("do stuff max 0");
        assert_eq!(goal, "do stuff");
        assert_eq!(max, 0);
    }

    #[test]
    fn parse_loop_start_args_max_non_numeric_defaults_zero() {
        let (goal, max) = parse_loop_start_args("go max abc");
        assert_eq!(goal, "go");
        assert_eq!(max, 0);
    }

    #[test]
    fn parse_loop_start_args_trims_goal() {
        let (goal, _) = parse_loop_start_args("   trim me   ");
        assert_eq!(goal, "trim me");
    }

    #[test]
    fn parse_loop_start_args_max_must_be_delimited_by_spaces() {
        let (goal, max) = parse_loop_start_args("watch max5");
        assert_eq!(goal, "watch max5");
        assert_eq!(max, 0);
    }

    #[test]
    fn parse_goal_args_no_suffix_defaults_twenty() {
        let (cond, max) = parse_goal_args("achieve X");
        assert_eq!(cond, "achieve X");
        assert_eq!(max, 20);
    }

    #[test]
    fn parse_goal_args_with_stop_after_n() {
        let (cond, max) = parse_goal_args("achieve X or stop after 5 turns");
        assert_eq!(cond, "achieve X");
        assert_eq!(max, 5);
    }

    #[test]
    fn parse_goal_args_stop_after_non_numeric_defaults_twenty() {
        let (cond, max) = parse_goal_args("X or stop after abc turns");
        assert_eq!(cond, "X");
        assert_eq!(max, 20);
    }

    #[test]
    fn parse_goal_args_case_insensitive_suffix() {
        let (cond, max) = parse_goal_args("X OR STOP AFTER 7 turns");
        assert_eq!(cond, "X");
        assert_eq!(max, 7);
    }

    #[test]
    fn cmd_loop_no_args_no_active_loop() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("No active loop"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_no_args_shows_active_loop_status() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 2,
            max_turns: 5,
        });
        let r = invoke(&mut app, "loop");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("Loop active"), "got {text:?}");
                assert!(text.contains("2/5"));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_no_args_unlimited_max_shown() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 3,
            max_turns: 0,
        });
        let r = invoke(&mut app, "loop");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("unlimited"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_start_emits_start_action_and_state() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop start watch the build");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions.len(), 1);
                match &actions[0] {
                    UserAction::StartLoop { goal, max_turns } => {
                        assert_eq!(goal, "watch the build");
                        assert_eq!(*max_turns, 0);
                    }
                    other => panic!("expected StartLoop, got {other:?}"),
                }
            }
            other => panic!("expected Async, got {other:?}"),
        }
        let ls = app.loop_state.as_ref().expect("loop_state set");
        assert_eq!(ls.goal, "watch the build");
        assert_eq!(ls.max_turns, 0);
        assert_eq!(ls.turns_run, 0);
    }

    #[test]
    fn cmd_loop_start_with_max_n() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop start watch max 3");
        match r {
            InvokeResult::Async(actions) => match &actions[0] {
                UserAction::StartLoop { goal, max_turns } => {
                    assert_eq!(goal, "watch");
                    assert_eq!(*max_turns, 3);
                }
                other => panic!("expected StartLoop, got {other:?}"),
            },
            other => panic!("expected Async, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_start_empty_goal_errors() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop start");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => {
                assert!(text.contains("Usage"), "got {text:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(app.loop_state.is_none(), "no state on usage error");
    }

    #[test]
    fn cmd_loop_stop_emits_stop_and_clears_state() {
        let mut app = App::new();
        app.loop_state = Some(crate::app::LoopUiState {
            goal: "g".into(),
            turns_run: 1,
            max_turns: 0,
        });
        let r = invoke(&mut app, "loop stop");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions, vec![UserAction::StopLoop]);
            }
            other => panic!("expected Async([StopLoop]), got {other:?}"),
        }
        assert!(app.loop_state.is_none());
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("Loop stopped"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_trigger_emits_manual_trigger() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop trigger check it");
        match r {
            InvokeResult::Async(actions) => match &actions[0] {
                UserAction::LoopTrigger { source, prompt } => {
                    assert_eq!(source, "manual");
                    assert_eq!(prompt, "check it");
                }
                other => panic!("expected LoopTrigger, got {other:?}"),
            },
            other => panic!("expected Async, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_trigger_empty_errors() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop trigger");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => {
                assert!(text.contains("Usage"), "got {text:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cmd_loop_unknown_subcommand_errors() {
        let mut app = App::new();
        let r = invoke(&mut app, "loop frobnicate");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => {
                assert!(text.contains("Unknown /loop sub-command"), "got {text:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn registry_includes_loop_command() {
        let r = CommandRegistry::default_set();
        assert!(r.lookup("loop").is_some(), "/loop should be registered");
        let spec = r.lookup("loop").unwrap();
        assert!(matches!(spec.handler, CommandHandler::Async(_)));
    }

    // ── Pre-existing coverage: CommandSpec Debug, lookup_skill, ─────────
    //    cmd_permissions, cmd_add_dir flags, cmd_goal status/clear.

    #[test]
    fn command_spec_debug_includes_all_fields() {
        let spec = CommandSpec {
            name: "frob",
            aliases: &["f", "fb"],
            summary: "frob a thing",
            usage: "/frob <x>",
            handler: CommandHandler::Sync(cmd_clear),
        };
        let s = format!("{spec:?}");
        assert!(s.contains("CommandSpec"), "got {s:?}");
        assert!(s.contains("frob"), "name missing: {s:?}");
        assert!(s.contains("frob a thing"), "summary missing: {s:?}");
        assert!(s.contains("/frob <x>"), "usage missing: {s:?}");
        assert!(s.contains("<fn>"), "handler placeholder missing: {s:?}");
    }

    fn sample_skill(name: &str, aliases: &[&str]) -> crate::skill_commands::SkillCommand {
        crate::skill_commands::SkillCommand {
            name: name.into(),
            description: "desc".into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            argument_hint: "".into(),
            allowed_tools: None,
            prompt_template: "do $ARGUMENTS".into(),
            source_path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn lookup_skill_returns_none_when_builtin_shadows() {
        let r = CommandRegistry::default_set().with_skill_commands(vec![sample_skill("help", &[])]);
        assert!(
            r.lookup_skill("help").is_none(),
            "built-in name shadows skill"
        );
        // A distinct alias not claimed by any built-in still resolves to the skill.
        let r2 = CommandRegistry::default_set()
            .with_skill_commands(vec![sample_skill("help", &["hlpx"])]);
        assert!(
            r2.lookup_skill("hlpx").is_some(),
            "non-built-in alias resolves"
        );
    }

    #[test]
    fn lookup_skill_finds_skill_by_name() {
        let r = CommandRegistry::default_set().with_skill_commands(vec![sample_skill("frob", &[])]);
        let s = r.lookup_skill("frob").expect("skill by name");
        assert_eq!(s.name, "frob");
    }

    #[test]
    fn lookup_skill_finds_skill_by_alias() {
        let r =
            CommandRegistry::default_set().with_skill_commands(vec![sample_skill("frob", &["fb"])]);
        let s = r.lookup_skill("fb").expect("skill by alias");
        assert_eq!(s.name, "frob");
    }

    #[test]
    fn lookup_skill_returns_none_for_unknown() {
        let r = CommandRegistry::default_set().with_skill_commands(vec![sample_skill("frob", &[])]);
        assert!(r.lookup_skill("nope").is_none());
    }

    #[test]
    fn cmd_permissions_on_enables_hook() {
        let mut app = App::new();
        for arg in ["on", "true", "1"] {
            let r = invoke(&mut app, &format!("permissions {arg}"));
            assert!(
                matches!(r, InvokeResult::Sync(CommandOutcome::Done)),
                "arg {arg}"
            );
            assert!(
                app.permission_hook_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                "arg {arg} should enable"
            );
        }
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => assert!(text.contains("on"), "got {text:?}"),
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_permissions_off_disables_and_clears_auto_allow() {
        let mut app = App::new();
        app.permission_hook_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        app.auto_allowed_tools.insert("Bash".into());
        for arg in ["off", "false", "0"] {
            let r = invoke(&mut app, &format!("permissions {arg}"));
            assert!(
                matches!(r, InvokeResult::Sync(CommandOutcome::Done)),
                "arg {arg}"
            );
            assert!(
                !app.permission_hook_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
                "arg {arg} should disable"
            );
            assert!(
                app.auto_allowed_tools.is_empty(),
                "arg {arg} should clear auto-allow"
            );
        }
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => assert!(text.contains("off"), "got {text:?}"),
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_permissions_no_arg_shows_usage_with_current_state() {
        let mut app = App::new();
        app.permission_hook_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let r = invoke(&mut app, "permissions");
        assert!(matches!(r, InvokeResult::Sync(CommandOutcome::Done)));
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => {
                assert!(text.contains("Usage"), "got {text:?}");
                assert!(text.contains("currently off"), "got {text:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cmd_add_dir_dash_dash_ro_flag_grants_readonly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        invoke(&mut app, &format!("add-dir {} --ro", tmp.path().display()));
        let roots = app.session_roots.read().expect("read").clone();
        assert_eq!(roots.len(), 1);
        assert!(matches!(roots[0].1, recursive::tools::AccessTier::ReadOnly));
    }

    #[test]
    fn cmd_add_dir_short_r_flag_grants_readonly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut app = App::new();
        invoke(&mut app, &format!("add-dir {} -r", tmp.path().display()));
        let roots = app.session_roots.read().expect("read").clone();
        assert_eq!(roots.len(), 1);
        assert!(matches!(roots[0].1, recursive::tools::AccessTier::ReadOnly));
    }

    #[test]
    fn cmd_goal_no_args_no_active_goal() {
        let mut app = App::new();
        let r = invoke(&mut app, "goal");
        assert!(matches!(r, InvokeResult::Async(a) if a.is_empty()));
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("No active goal"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_goal_clear_emits_clear_action() {
        let mut app = App::new();
        let r = invoke(&mut app, "goal clear");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions, vec![UserAction::ClearGoal]);
            }
            other => panic!("expected Async([ClearGoal]), got {other:?}"),
        }
        assert!(app.active_goal.is_none());
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("Goal cleared"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn cmd_goal_clear_is_case_insensitive() {
        let mut app = App::new();
        let r = invoke(&mut app, "goal CLEAR");
        assert_eq!(
            match r {
                InvokeResult::Async(a) => a,
                _ => vec![],
            },
            vec![UserAction::ClearGoal]
        );
    }

    #[test]
    fn cmd_goal_set_emits_set_goal_with_parsed_max() {
        let mut app = App::new();
        let r = invoke(&mut app, "goal achieve X or stop after 5 turns");
        match r {
            InvokeResult::Async(actions) => match &actions[0] {
                UserAction::SetGoal {
                    condition,
                    max_turns,
                } => {
                    assert_eq!(condition, "achieve X");
                    assert_eq!(*max_turns, 5);
                }
                other => panic!("expected SetGoal, got {other:?}"),
            },
            other => panic!("expected Async, got {other:?}"),
        }
        match app.blocks.last() {
            Some(TranscriptBlock::System { text }) => {
                assert!(text.contains("Goal set"), "got {text:?}");
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    // ── Pre-existing: build_*_lines / serde_*_context renderers ─────────

    fn text_of(lines: &[ratatui::text::Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref().to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn build_cost_lines_computes_per_token_costs() {
        use crate::cost::UsageStats;
        let usage = UsageStats {
            total_input: 1_000_000,
            total_output: 2_000_000,
            last_latency_ms: 1_500,
            ..Default::default()
        };
        let model = "MiniMax-M3";
        let pricing = recursive::llm::pricing_for(model).expect("MiniMax-M3 has pricing");
        let cost_in = 1_000_000.0 * pricing.input_per_million / 1_000_000.0;
        let cost_out = 2_000_000.0 * pricing.output_per_million / 1_000_000.0;
        let cost_total = cost_in + cost_out;

        let text = text_of(&build_cost_lines(&usage, model));
        assert!(
            text.contains(&format!("(${cost_in:.4})")),
            "input cost missing in {text:?}"
        );
        assert!(
            text.contains(&format!("(${cost_out:.4})")),
            "output cost missing in {text:?}"
        );
        assert!(
            text.contains(&format!("(${cost_total:.4})")),
            "total cost missing in {text:?}"
        );
        assert!(text.contains("Provider         : MiniMax-M3"));
    }

    #[test]
    fn build_model_lines_renders_model_provider_endpoint() {
        let lines = build_model_lines();
        assert!(lines.len() > 3, "model panel should have several lines");
        let text = text_of(&lines);
        assert!(text.contains("Model    :"), "got {text:?}");
        assert!(text.contains("Provider :"), "got {text:?}");
        assert!(text.contains("Endpoint :"), "got {text:?}");
    }

    #[test]
    fn build_model_lines_warns_when_no_api_key_configured() {
        // Pin RECURSIVE_HOME to an empty temp dir and clear the generic
        // API-key env vars so `Config::from_env` resolves no provider —
        // the same condition that leaves the TUI offline. The panel must
        // surface a "Not configured" banner rather than presenting the
        // hardcoded `deepseek-v4-flash` fallback as a live configuration.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let text = text_of(&build_model_lines());
        assert!(
            text.contains("Not configured"),
            "expected not-configured banner, got: {text:?}"
        );
        assert!(
            text.contains("recursive init"),
            "banner should point to the wizard, got: {text:?}"
        );

        match prev_recursive {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }

    #[test]
    fn build_model_lines_warns_when_api_key_is_empty_string() {
        // An empty-string api_key is treated as "not configured" (the runtime
        // builder's `effective_api_key` filters empty keys). This pins the
        // `!` in `.map(|k| !k.is_empty())`: a `delete !` mutant would treat
        // an empty key as configured and hide the banner.
        let empty_home = tempfile::tempdir().expect("tempdir");
        let _pin = recursive::test_util::PinnedRecursiveHome::new(empty_home.path());
        let prev_recursive = std::env::var("RECURSIVE_API_KEY").ok();
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("RECURSIVE_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        let cfg_dir = empty_home.path().join(".recursive");
        std::fs::create_dir_all(&cfg_dir).expect("mkdir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[provider]\napi_key = \"\"\nmodel = \"deepseek-v4-flash\"\n",
        )
        .expect("write config");

        let text = text_of(&build_model_lines());
        assert!(
            text.contains("Not configured"),
            "empty-string api_key should trigger the not-configured banner, got: {text:?}"
        );

        match prev_recursive {
            Some(v) => std::env::set_var("RECURSIVE_API_KEY", v),
            None => std::env::remove_var("RECURSIVE_API_KEY"),
        }
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }

    #[test]
    fn build_tool_lines_truncates_long_descriptions_at_60_chars() {
        // 61-char description: `> 60` true → truncated with ellipsis.
        let long = (0..61).map(|_| 'x').collect::<String>();
        let lines = build_tool_lines(&[("Tool61".to_string(), long.clone())]);
        let text = text_of(&lines);
        assert!(
            text.contains('…'),
            "61-char desc should be ellipsised: {text:?}"
        );
        assert!(text.contains("Available tools (1)"));

        // 60-char description: `> 60` false → no ellipsis. Kills `>=`/`==`.
        let exact = (0..60).map(|_| 'y').collect::<String>();
        let lines60 = build_tool_lines(&[("Tool60".to_string(), exact.clone())]);
        let text60 = text_of(&lines60);
        assert!(
            !text60.contains('…'),
            "60-char desc should not be ellipsised: {text60:?}"
        );
        assert!(text60.contains(&exact));
    }

    #[test]
    fn build_tool_lines_empty_state_message() {
        let lines = build_tool_lines(&[]);
        let text = text_of(&lines);
        assert!(text.contains("(no tools registered)"), "got {text:?}");
        assert!(text.contains("Available tools (0)"));
    }

    fn journal(name: &str, preview: &str) -> crate::ui::modal::JournalEntry {
        crate::ui::modal::JournalEntry {
            name: name.to_string(),
            preview: preview.to_string(),
        }
    }

    #[test]
    fn build_journal_lines_marks_and_styles_selected_entry() {
        let entries = vec![journal("alpha", "body"), journal("beta", "body")];
        let lines = build_journal_lines(&entries, 0);
        assert!(lines.len() > 3, "got {} lines", lines.len());
        let text = text_of(&lines);
        assert!(text.contains("Recent journal entries"));
        assert!(text.contains("▶"), "selected marker missing: {text:?}");
        // The selected entry's name span (spans[1]) must be yellow+bold.
        // This kills the `i == selected` -> `!=` style mutant (969).
        let selected_line = lines
            .iter()
            .find(|l| text_of(std::slice::from_ref(l)).contains("▶"))
            .expect("selected line");
        let name_span = &selected_line.spans[1];
        assert_eq!(
            name_span.style,
            ratatui::style::Style::default()
                .fg(ratatui::style::Color::Yellow)
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn build_journal_lines_truncates_preview_over_12_lines() {
        let long_preview = (0..13)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entries = vec![journal("big", &long_preview)];
        let text = text_of(&build_journal_lines(&entries, 0));
        assert!(
            text.contains("more lines)"),
            "13-line preview should announce truncation: {text:?}"
        );

        let exact_preview = (0..12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entries12 = vec![journal("exact", &exact_preview)];
        let text12 = text_of(&build_journal_lines(&entries12, 0));
        assert!(
            !text12.contains("more lines)"),
            "12-line preview should not be truncated: {text12:?}"
        );
    }

    #[test]
    fn build_journal_lines_empty_state_message() {
        let lines = build_journal_lines(&[], 0);
        let text = text_of(&lines);
        assert!(text.contains("no entries in .dev/journal/"), "got {text:?}");
    }

    #[test]
    fn serde_journal_context_joins_names_with_newlines() {
        let entries = vec![journal("a", "x"), journal("b", "y")];
        assert_eq!(serde_journal_context(&entries), "a\nb");
        assert_eq!(serde_journal_context(&[]), "");
    }

    fn resume(slug: &str) -> crate::ui::modal::ResumeEntry {
        crate::ui::modal::ResumeEntry {
            session_dir: std::path::PathBuf::from(format!("/tmp/{slug}")),
            slug: slug.to_string(),
            updated_at: "2026-06-01 14:22".to_string(),
            turn_count: 3,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn build_resume_lines_marks_and_styles_selected_session() {
        let entries = vec![resume("first"), resume("second")];
        let lines = build_resume_lines(&entries, 1);
        assert!(lines.len() > 3, "got {} lines", lines.len());
        let text = text_of(&lines);
        assert!(text.contains("Recent sessions"));
        assert!(text.contains("▶"), "selected marker missing: {text:?}");
        assert!(text.contains("turns:  3"));
        // Selected entry's line span is yellow+bold — kills `==` -> `!=` (1026).
        let selected_line = lines
            .iter()
            .find(|l| text_of(std::slice::from_ref(l)).contains("▶"))
            .expect("selected line");
        assert_eq!(
            selected_line.spans[0].style,
            ratatui::style::Style::default()
                .fg(ratatui::style::Color::Yellow)
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn serde_resume_context_joins_session_dirs_with_newlines() {
        let entries = vec![resume("first"), resume("second")];
        assert_eq!(serde_resume_context(&entries), "/tmp/first\n/tmp/second");
        assert_eq!(serde_resume_context(&[]), "");
    }

    #[test]
    fn build_theme_picker_lines_marks_selected_theme() {
        let lines = build_theme_picker_lines("default", 0);
        assert!(lines.len() > 2, "got {} lines", lines.len());
        let text = text_of(&lines);
        assert!(text.contains("Choose theme  (current: default)"));
        assert!(
            text.contains("▶"),
            "selected theme marker missing: {text:?}"
        );
        // The ▶ line's span must be yellow+bold — kills `i == selected` ->
        // `!=` (1069), which would give the selected row the white style.
        let selected_line = lines
            .iter()
            .find(|l| text_of(std::slice::from_ref(l)).contains('▶'))
            .expect("selected line");
        assert_eq!(
            selected_line.spans[0].style,
            ratatui::style::Style::default()
                .fg(ratatui::style::Color::Yellow)
                .add_modifier(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn build_help_lines_lists_skill_commands_when_present() {
        use crate::skill_commands::SkillCommand;
        let mut registry = CommandRegistry::default_set();
        registry.set_skill_commands(vec![SkillCommand {
            name: "my-skill".to_string(),
            description: "does a thing".to_string(),
            aliases: vec![],
            argument_hint: String::new(),
            allowed_tools: None,
            prompt_template: String::new(),
            source_path: std::path::PathBuf::from("/tmp/my-skill"),
        }]);
        let text = text_of(&build_help_lines(&registry));
        assert!(
            text.contains("Skill Commands:"),
            "skill section header missing: {text:?}"
        );
        assert!(text.contains("my-skill"), "skill name missing: {text:?}");
    }

    #[test]
    fn build_help_lines_omits_skill_section_when_empty() {
        let registry = CommandRegistry::default_set();
        let text = text_of(&build_help_lines(&registry));
        assert!(
            !text.contains("Skill Commands:"),
            "empty skill list should not render the section header: {text:?}"
        );
    }

    #[test]
    fn cmd_mcp_emits_list_mcp_servers_action() {
        let mut app = App::new();
        let actions = cmd_mcp(&mut app, &[]);
        assert_eq!(actions, vec![UserAction::ListMcpServers]);
    }

    #[test]
    fn cmd_theme_no_args_selects_current_theme_row() {
        // Kills the `t.name == current` -> `!=` mutant in cmd_theme (714):
        // the panel's `selected` must be the index of the current theme in
        // ALL_THEMES, not the first theme that differs from it.
        use crate::ui::theme::ALL_THEMES;
        let mut app = App::new();
        let expected = ALL_THEMES
            .iter()
            .position(|t| t.name == app.theme.name)
            .unwrap_or(0);
        let r = invoke(&mut app, "theme");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenPanel(panel)) => {
                assert_eq!(
                    panel.selected,
                    Some(expected),
                    "theme picker should open with the current theme selected"
                );
            }
            other => panic!("expected OpenPanel for /theme, got {other:?}"),
        }
    }

    #[test]
    fn cmd_goal_single_non_clear_arg_sets_goal() {
        // Kills `args.len() == 1 && args[0].eq_ignore_ascii_case("clear")`
        // -> `||` (550): with a single non-"clear" arg the original sets a
        // goal, the mutant would clear it.
        let mut app = App::new();
        let r = invoke(&mut app, "goal hello");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions.len(), 1);
                match &actions[0] {
                    UserAction::SetGoal {
                        condition,
                        max_turns,
                    } => {
                        assert_eq!(condition, "hello");
                        assert_eq!(*max_turns, 20);
                    }
                    other => panic!("expected SetGoal, got {other:?}"),
                }
            }
            other => panic!("expected Async (SetGoal), got {other:?}"),
        }
    }
}

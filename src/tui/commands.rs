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

use crate::tui::app::App as AppState;
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
#[derive(Debug, Clone, PartialEq)]
pub enum CommandOutcome {
    /// Handler completed; nothing more for the dispatcher to do.
    Done,
    /// Push an error block describing why the command failed.
    Error(String),
    /// Push a modal onto the stack.
    OpenModal(Modal),
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

fn cmd_help(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    CommandOutcome::OpenModal(Modal::Help)
}

fn cmd_clear(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    app.reset_transcript();
    CommandOutcome::Done
}

fn cmd_compact(app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    app.push_system("Compacting transcript…");
    vec![UserAction::Compact]
}

fn cmd_cost(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    CommandOutcome::OpenModal(Modal::CostDetail)
}

fn cmd_model(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    CommandOutcome::OpenModal(Modal::ModelInfo)
}

fn cmd_status(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let uptime_secs = app.start_time.elapsed().as_secs();
    let total_tokens = app.usage.total_input.saturating_add(app.usage.total_output);
    let plan = if app.planning_mode_on { "on" } else { "off" };
    let text = format!(
        "Status — turn {}, blocks {}, tokens {}, uptime {}s, planning {}",
        app.turn_count,
        app.blocks.len(),
        total_tokens,
        uptime_secs,
        plan
    );
    app.push_system(text);
    CommandOutcome::Done
}

fn cmd_tools(app: &mut AppState, _args: &[String]) -> CommandOutcome {
    CommandOutcome::OpenModal(Modal::ToolList {
        entries: app.tool_catalog.clone(),
    })
}

fn cmd_plan(app: &mut AppState, args: &[String]) -> Vec<UserAction> {
    let arg = args.first().map(|s| s.to_lowercase());
    let on = match arg.as_deref() {
        Some("on") | Some("true") | Some("1") => true,
        Some("off") | Some("false") | Some("0") => false,
        _ => {
            app.push_error("Usage: /plan on|off");
            return Vec::new();
        }
    };
    app.planning_mode_on = on;
    app.push_system(format!("Planning mode: {}", if on { "on" } else { "off" }));
    vec![UserAction::SetPlanningMode(on)]
}

fn cmd_journal(_app: &mut AppState, _args: &[String]) -> CommandOutcome {
    let entries = crate::tui::ui::modal::load_recent_journal_entries(5);
    CommandOutcome::OpenModal(Modal::Journal {
        entries,
        selected: 0,
    })
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
    use crate::tui::ui::modal::{load_recent_sessions, ResumeEntry};
    let workspace = app.workspace_path.clone();
    let entries: Vec<ResumeEntry> = load_recent_sessions(&workspace, 20);
    if entries.is_empty() {
        CommandOutcome::Error("No saved sessions found.".into())
    } else {
        CommandOutcome::OpenModal(Modal::ResumePicker {
            entries,
            selected: 0,
        })
    }
}

fn cmd_mcp(_app: &mut AppState, _args: &[String]) -> Vec<UserAction> {
    vec![UserAction::ListMcpServers]
}

fn cmd_theme(app: &mut AppState, args: &[String]) -> CommandOutcome {
    use crate::tui::ui::theme::ALL_THEMES;
    let theme_list: Vec<&str> = ALL_THEMES.iter().map(|t| t.name).collect();
    if args.is_empty() {
        let names = theme_list.join(", ");
        app.push_system(format!(
            "Current theme: {}. Available: {}",
            app.theme.name, names
        ));
        return CommandOutcome::Done;
    }
    let requested = args[0].to_lowercase();
    let found = crate::tui::ui::theme::find_theme(&requested);
    if found.name == requested {
        app.theme = found;
        app.push_system(format!("Theme switched to '{}'.", found.name));
    } else {
        let names = theme_list.join(", ");
        app.push_error(format!(
            "Unknown theme '{}'. Available: {}",
            requested, names
        ));
    }
    CommandOutcome::Done
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
    fn cmd_theme_no_args_prints_current() {
        let mut app = App::new();
        let blocks_before = app.blocks.len();
        invoke(&mut app, "theme");
        assert!(app.blocks.len() > blocks_before);
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
                assert!(text.contains("planning off"));
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
    fn plan_on_off_toggles_state_and_pushes_system_block() {
        let mut app = App::new();
        let r = invoke(&mut app, "plan on");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions, vec![UserAction::SetPlanningMode(true)]);
            }
            other => panic!("expected async actions, got {other:?}"),
        }
        assert!(app.planning_mode_on);

        let r = invoke(&mut app, "plan off");
        match r {
            InvokeResult::Async(actions) => {
                assert_eq!(actions, vec![UserAction::SetPlanningMode(false)]);
            }
            other => panic!("expected async actions, got {other:?}"),
        }
        assert!(!app.planning_mode_on);
    }

    #[test]
    fn plan_without_arg_pushes_error_and_no_action() {
        let mut app = App::new();
        let r = invoke(&mut app, "plan");
        match r {
            InvokeResult::Async(actions) => {
                assert!(actions.is_empty(), "expected no actions, got {actions:?}");
            }
            other => panic!("expected async result, got {other:?}"),
        }
        match app.blocks.last() {
            Some(TranscriptBlock::Error { text }) => assert!(text.contains("Usage")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn help_opens_help_modal() {
        let mut app = App::new();
        let r = invoke(&mut app, "help");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenModal(Modal::Help)) => {}
            other => panic!("expected OpenModal(Help), got {other:?}"),
        }
    }

    #[test]
    fn cost_opens_cost_modal() {
        let mut app = App::new();
        let r = invoke(&mut app, "cost");
        assert!(matches!(
            r,
            InvokeResult::Sync(CommandOutcome::OpenModal(Modal::CostDetail))
        ));
    }

    #[test]
    fn tools_opens_modal_with_catalog() {
        let mut app = App::new();
        let r = invoke(&mut app, "tools");
        match r {
            InvokeResult::Sync(CommandOutcome::OpenModal(Modal::ToolList { entries })) => {
                assert!(entries.iter().any(|(n, _)| n == "read_file"));
            }
            other => panic!("expected OpenModal(ToolList), got {other:?}"),
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

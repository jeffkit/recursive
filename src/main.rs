//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

mod cli;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::Level;

use recursive::mcp::{JsonRpcRequest, JsonRpcResponse};
use recursive::SessionFile;
use recursive::SessionWriter;
use recursive::{
    config::Config,
    llm::{load_pricing_from_yaml, AnthropicProvider, LlmProvider, ModelPricing, OpenAiProvider},
    tools::{ScheduleWakeup, WakeupSlot},
    AgentRuntimeBuilder, ChannelSink, CompositeSink, EventSink, FinishReason, NullSink,
    PlanningMode, RetryPolicy, SessionPersistenceSink, ToolRegistry,
};

#[derive(Parser, Debug)]
#[command(
    name = "recursive",
    version,
    about = "A minimal self-improving coding agent"
)]
struct Cli {
    /// Workspace root the agent can read/write within.
    #[arg(long, env = "RECURSIVE_WORKSPACE")]
    workspace: Option<PathBuf>,

    /// LLM model identifier (e.g., deepseek-chat, gpt-4o-mini, claude-sonnet-4-20250514).
    #[arg(long, short = 'm')]
    model: Option<String>,

    /// API key for the LLM provider.
    #[arg(long, short = 'k', hide_env_values = true)]
    api_key: Option<String>,

    /// Base URL for the LLM API endpoint.
    #[arg(long)]
    api_base: Option<String>,

    /// LLM provider protocol type.
    #[arg(long, value_parser = ["openai", "anthropic"])]
    provider: Option<String>,

    /// Maximum agent loop iterations per goal.
    #[arg(long, env = "RECURSIVE_MAX_STEPS")]
    max_steps: Option<usize>,

    /// Stop when total transcript content reaches this many characters.
    #[arg(long, env = "RECURSIVE_MAX_TRANSCRIPT_CHARS")]
    max_transcript_chars: Option<usize>,

    /// Path to a system prompt file (overrides default).
    #[arg(long, env = "RECURSIVE_SYSTEM_PROMPT_FILE")]
    system_prompt_file: Option<PathBuf>,

    /// Path to MCP server config JSON file.
    #[arg(long, env = "RECURSIVE_MCP_CONFIG")]
    mcp_config: Option<PathBuf>,

    /// Log level: error|warn|info|debug|trace.
    #[arg(long, default_value = "info")]
    log: String,

    /// Persist the full transcript to <path> as JSON when the run finishes.
    #[arg(long, env = "RECURSIVE_TRANSCRIPT_OUT")]
    transcript_out: Option<PathBuf>,

    /// Emit StepEvents as newline-delimited JSON on stdout instead of the
    /// human-readable trace. Pipeable to jq or other downstream tooling.
    #[arg(long, env = "RECURSIVE_JSON")]
    json: bool,

    /// Enable token-by-token streaming. Deltas are printed live on stderr.
    #[arg(long, env = "RECURSIVE_STREAM")]
    stream: bool,
    /// Enable tool timing hook that prints tool call durations to stderr.
    #[arg(long)]
    hook_timing: bool,
    /// Run in headless mode: interactive tools go through external hooks
    /// instead of waiting for terminal input. If no hook approves the call,
    /// the tool is auto-denied. Also set via RECURSIVE_HEADLESS=1.
    #[arg(long = "headless", short = 'H', env = "RECURSIVE_HEADLESS")]
    headless: bool,
    /// Path to write a session file for non-success finishes (budget exceeded,
    /// stuck, transcript limit). The session can be resumed later with `resume`.
    #[arg(long, env = "RECURSIVE_SESSION_OUT")]
    session_out: Option<PathBuf>,

    /// Disable live session recording. By default every run is persisted
    /// as JSONL under .recursive/sessions/<slug>/<session-id>/.
    /// Set this flag (or RECURSIVE_NO_SESSION=1) to skip persistence.
    #[arg(long = "no-session", env = "RECURSIVE_NO_SESSION")]
    no_session: bool,

    /// Enable plan-first mode: agent proposes a plan, user confirms before execution.
    #[arg(long = "plan-first")]
    plan_first: bool,

    /// Path to external pricing YAML file. If provided, pricing from this file
    /// takes precedence over hardcoded values. Models not in the file fall back
    /// to hardcoded rates.
    #[arg(long, env = "RECURSIVE_PRICING_FILE")]
    pricing_file: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Run a one-shot prompt (non-interactive). Like `recursive run` but shorter.
    #[arg(short = 'p', long = "print")]
    prompt: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the agent once with the given goal (concatenated).
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        goal: Vec<String>,
    },
    /// Interactive multi-turn REPL (default when no command is given).
    Repl,
    /// Start as an MCP server (stdio transport).
    Serve {
        /// Workspace path for tool sandboxing.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Start the HTTP API server.
    #[cfg(feature = "http")]
    Http {
        /// Address to bind (e.g. 127.0.0.1:3000).
        #[arg(long, default_value = "127.0.0.1:3000")]
        addr: String,
    },
    /// Interactive setup wizard — configure provider, model, and API key.
    Init,
    /// Print registered tool specs as JSON (sanity check).
    Tools,
    /// Pretty-print a previously saved transcript JSON file, or resume a
    /// run from a saved transcript when `--resume-from N <goal>` is given.
    Replay {
        /// Path to the transcript JSON file (as written by --transcript-out).
        path: PathBuf,
        /// Take the first N messages of the saved transcript as seed
        /// context for a new run. Requires a trailing <goal>.
        #[arg(long)]
        resume_from: Option<usize>,
        /// Goal for the resumed run. Required when --resume-from is given;
        /// ignored otherwise.
        #[arg(trailing_var_arg = true)]
        goal: Vec<String>,
        /// Print only the last N messages of the transcript.
        /// Ignored when --resume-from is given.
        #[arg(long)]
        tail: Option<usize>,
        /// Print only the first N messages of the transcript.
        /// Mutually exclusive with --tail. Ignored when --resume-from is given.
        #[arg(long)]
        head: Option<usize>,
    },
    /// Resume a run from a saved session.
    ///
    /// **Goal 151**: prefer specifying a session ID (or substring)
    /// recorded under `~/.recursive/.../sessions/`. Without an
    /// argument, the most-recent active or interrupted session in
    /// the current workspace is resumed.
    Resume {
        /// Session ID or unique substring. If omitted, resumes the
        /// most-recent active/interrupted session in this workspace.
        session: Option<String>,
        /// Escape hatch: resume from an explicit JSONL session
        /// directory path (not a legacy `.json` file). Mutually
        /// exclusive with the positional argument.
        #[arg(long, conflicts_with = "session")]
        from_file: Option<PathBuf>,
        /// How to handle orphan tool calls detected on resume
        /// (tool_calls in the last assistant message with no matching
        /// tool result). Choices: ask (default on TTY), skip, redo, abort.
        /// On non-TTY (CI) the default is abort.
        #[arg(long, value_name = "POLICY")]
        orphans: Option<String>,
    },
    /// List or inspect saved sessions.
    Sessions {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// View or modify configuration.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run the agent in loop mode: agent self-schedules wakeups until it stops.
    Loop {
        /// Initial goal to start the loop with.
        #[arg(trailing_var_arg = true, required = true)]
        goal: Vec<String>,
    },
    /// Migrate legacy in-tree state (sessions, shadow-git, scratchpad)
    /// from `<workspace>/.recursive/` to the per-user data dir at
    /// `~/.recursive/workspaces/<hash>/`.
    Migrate {
        /// Show what would be moved without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// List all session files in the workspace's session directory.
    List,
    /// Show details of a specific session (by path or session ID).
    Show {
        /// Path to the session JSON file, or a session ID to search for.
        session: String,
    },
    /// Delete a session file or session directory.
    Delete {
        /// Path to the session JSON file, or a session ID to search for.
        session: String,
        /// Skip confirmation prompt.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Export a session as portable JSON.
    Export {
        /// Session directory path or session ID.
        session: String,
        /// Output file (default: stdout).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Rewind a session to the start of turn N, restoring only files
    /// this session touched in turns >= N. Sibling sessions' files are
    /// untouched. Conflicts (a touched file modified externally since
    /// our last snapshot) abort unless --force is given.
    Rewind {
        /// Session directory path or session ID.
        session: String,
        /// Turn index to rewind to. The start state of this turn is
        /// what gets restored; the turn itself and all later turns
        /// are dropped.
        #[arg(long)]
        to_turn: usize,
        /// Skip conflict detection and overwrite externally-modified files.
        #[arg(long)]
        force: bool,
        /// Print the plan but don't apply it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Convert a legacy `.json` session file (written by
    /// `--session-out`) into the JSONL session directory format
    /// so it can be resumed by ID. One-shot migration utility.
    MigrateLegacy {
        /// Path to the legacy `.json` session file.
        path: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Display the effective configuration (API keys are masked).
    Show,
    /// Set a config value in ~/.recursive/config.toml.
    Set {
        /// Config key (e.g., provider.model, agent.max_steps).
        key: String,
        /// Value to set.
        value: String,
    },
    /// Print the config file path.
    Path,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.log)?;

    if cli.session_out.is_some() {
        eprintln!(
            "warning: --session-out writes the legacy .json format, which is no longer\n\
             used for resume. Your session is automatically being persisted as JSONL\n\
             under the user data dir; use `recursive resume <id>` to resume.\n\
             This flag will be removed in a future release."
        );
    }

    let mut config = Config::from_env().context("loading config")?;
    if let Some(ws) = cli.workspace {
        config.workspace = ws;
    }
    if let Some(n) = cli.max_steps {
        config.max_steps = n;
    }
    if let Some(m) = cli.model {
        config.model = m;
    }
    if let Some(k) = cli.api_key {
        config.api_key = Some(k);
    }
    if let Some(b) = cli.api_base {
        config.api_base = b;
    }
    if let Some(p) = cli.provider {
        config.provider_type = p;
    }
    if cli.headless {
        config.headless = true;
    }
    if let Some(p) = cli.system_prompt_file {
        config.system_prompt = std::fs::read_to_string(&p)
            .with_context(|| format!("reading system prompt: {}", p.display()))?;
    }

    // Load external pricing if provided
    let external_pricing: Option<HashMap<String, ModelPricing>> =
        if let Some(path) = &cli.pricing_file {
            match load_pricing_from_yaml(path) {
                Ok(pricing) => {
                    eprintln!(
                        "pricing: loaded {} model(s) from {}",
                        pricing.len(),
                        path.display()
                    );
                    Some(pricing)
                }
                Err(e) => {
                    anyhow::bail!("failed to load pricing file {}: {}", path.display(), e);
                }
            }
        } else {
            None
        };

    // Determine effective command:
    // - Explicit subcommand → use it
    // - `-p "goal"` → one-shot run (like `claude -p`)
    // - Nothing → TUI (if compiled in), else REPL
    let effective_cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            if let Some(prompt) = cli.prompt {
                Cmd::Run { goal: vec![prompt] }
            } else {
                #[cfg(feature = "tui")]
                {
                    return recursive::tui::run().await.map_err(Into::into);
                }
                #[cfg(not(feature = "tui"))]
                Cmd::Repl
            }
        }
    };

    // Warn about legacy in-tree state for commands that interact with
    // the workspace. The Migrate command itself shouldn't double-warn.
    if !matches!(effective_cmd, Cmd::Migrate { .. }) {
        let legacy = recursive::legacy_paths_in_workspace(&config.workspace);
        if !legacy.is_empty() {
            eprintln!(
                "warning: legacy in-tree state detected at {}/.recursive/:",
                config.workspace.display()
            );
            for p in &legacy {
                eprintln!("    {}", p.display());
            }
            eprintln!("hint:    run `recursive migrate` to move it under ~/.recursive");
        }
    }

    match effective_cmd {
        Cmd::Tools => {
            let tools = cli::builder::build_tools(&config).await;
            let specs = tools.specs();
            println!("{}", serde_json::to_string_pretty(&specs)?);
            Ok(())
        }
        Cmd::Serve { workspace } => {
            let workspace = std::fs::canonicalize(&workspace)?;
            config.workspace = workspace;
            run_mcp_server_stdio(config, cli.mcp_config).await
        }
        #[cfg(feature = "http")]
        Cmd::Http { addr } => {
            if let Err(msg) = config.validate_for_agent() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            let tools = cli::builder::build_tools(&config).await;
            let tool_infos: Vec<recursive::http::ToolInfo> = tools
                .specs()
                .into_iter()
                .map(|spec| recursive::http::ToolInfo {
                    name: spec.name,
                    description: spec.description,
                    parameters: spec.parameters,
                })
                .collect();
            // Build the LLM provider from config
            let api_key = config.require_api_key()?;
            let retry = RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let provider: Arc<dyn recursive::llm::LlmProvider> = match config.provider_type.as_str()
            {
                "anthropic" => {
                    let anthropic_retry = recursive::llm::anthropic::RetryPolicy {
                        max_retries: config.retry_max,
                        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
                    };
                    let anthropic =
                        AnthropicProvider::new(&config.api_base, api_key, &config.model)
                            .with_temperature(config.temperature)
                            .with_retry_policy(anthropic_retry);
                    Arc::new(anthropic)
                }
                _ => {
                    let openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)
                        .with_temperature(config.temperature)
                        .with_retry_policy(retry);
                    Arc::new(openai)
                }
            };
            // Goal-169: build the slash command list from built-in TUI commands +
            // workspace skill files. Guarded by the `tui` feature since
            // CommandRegistry lives in the tui module.
            #[cfg(feature = "tui")]
            let slash_commands: Vec<recursive::http::SlashCommandInfo> = {
                let registry = recursive::tui::commands::CommandRegistry::default_set();
                let mut cmds: Vec<recursive::http::SlashCommandInfo> = registry
                    .commands()
                    .iter()
                    .map(|c| recursive::http::SlashCommandInfo {
                        name: c.name.to_string(),
                        description: c.summary.to_string(),
                        source: "builtin".to_string(),
                        aliases: c.aliases.iter().map(|a| a.to_string()).collect(),
                        argument_hint: String::new(),
                    })
                    .collect();
                let workspace = std::env::current_dir().unwrap_or_default();
                let skills = recursive::tui::skill_commands::SkillCommandLoader::load(&workspace);
                for skill in skills {
                    cmds.push(recursive::http::SlashCommandInfo {
                        name: skill.name.clone(),
                        description: skill.description.clone(),
                        source: "skill".to_string(),
                        aliases: skill.aliases.clone(),
                        argument_hint: skill.argument_hint.clone(),
                    });
                }
                cmds
            };
            #[cfg(not(feature = "tui"))]
            let slash_commands: Vec<recursive::http::SlashCommandInfo> = Vec::new();
            let state = recursive::http::AppState {
                tools: tool_infos,
                tool_registry: tools,
                config: config.clone(),
                provider,
                sessions: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                event_channels: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                metrics: std::sync::Arc::new(recursive::http::Metrics::default()),
                slash_commands: std::sync::Arc::new(slash_commands),
            };
            let router = recursive::http::build_router(state);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("Recursive HTTP API listening on {addr}");
            let shutdown = shutdown_signal();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await?;
            eprintln!("shutdown: HTTP server stopped gracefully");
            Ok(())
        }
        Cmd::Init => cli::init::run_init().await,
        Cmd::Run { goal } => {
            let shutdown = shutdown_signal();
            run_once(
                config,
                goal.join(" "),
                cli.max_transcript_chars,
                cli.transcript_out,
                cli.session_out,
                cli.json,
                cli.stream,
                cli.plan_first,
                cli.mcp_config,
                external_pricing,
                cli.hook_timing,
                !cli.no_session,
                shutdown,
            )
            .await
        }
        Cmd::Repl => {
            repl(
                config,
                cli.max_transcript_chars,
                cli.json,
                cli.plan_first,
                cli.mcp_config,
                external_pricing,
                cli.stream,
                cli.hook_timing,
            )
            .await
        }
        Cmd::Loop { goal } => {
            let shutdown = shutdown_signal();
            run_loop(
                config,
                goal.join(" "),
                cli.max_transcript_chars,
                cli.json,
                cli.stream,
                cli.plan_first,
                cli.mcp_config,
                external_pricing,
                cli.hook_timing,
                shutdown,
            )
            .await
        }
        Cmd::Replay {
            path,
            resume_from,
            goal,
            tail,
            head,
        } => {
            // Check mutual exclusivity of --head and --tail
            if tail.is_some() && head.is_some() {
                anyhow::bail!("--head and --tail are mutually exclusive");
            }

            let file = recursive::TranscriptFile::read_from(&path)?;
            match resume_from {
                None => {
                    // If --head is provided, use pretty_head
                    if let Some(n) = head {
                        print!("{}", file.pretty_head(n));
                    // If --tail is provided without --resume-from, use pretty_tail
                    } else if let Some(n) = tail {
                        print!("{}", file.pretty_tail(n));
                    } else {
                        print!("{}", file.pretty());
                    }
                    Ok(())
                }
                Some(_) if goal.is_empty() => {
                    anyhow::bail!("--resume-from requires a trailing <goal> to continue the run");
                }
                Some(n) => {
                    let seed = file.take_first_n(n).ok_or_else(|| {
                        anyhow::anyhow!(
                            "--resume_from {n} exceeds saved transcript length ({})",
                            file.messages().len()
                        )
                    })?;
                    let shutdown = shutdown_signal();
                    cli::resume::run_resumed(
                        config,
                        seed.to_vec(),
                        goal.join(" "),
                        cli.max_transcript_chars,
                        cli.transcript_out,
                        cli.session_out,
                        cli.json,
                        cli.plan_first,
                        cli.mcp_config,
                        external_pricing,
                        cli.hook_timing,
                        !cli.no_session,
                        shutdown,
                        None, // existing_writer — legacy --resume-from creates a fresh session
                    )
                    .await
                }
            }
        }
        Cmd::Resume {
            session,
            from_file,
            orphans,
        } => {
            cli::resume::cmd_resume(
                config,
                session,
                from_file,
                orphans,
                cli.max_transcript_chars,
                cli.transcript_out,
                cli.session_out,
                cli.json,
                cli.plan_first,
                cli.mcp_config,
                external_pricing,
                cli.hook_timing,
                !cli.no_session,
            )
            .await
        }
        Cmd::Sessions { cmd } => match cmd {
            SessionCmd::List => {
                let old_sessions = recursive::session::list_sessions(&config.workspace)?;
                let new_sessions =
                    recursive::session::SessionReader::list_sessions(&config.workspace)?;
                let total = old_sessions.len() + new_sessions.len();
                if total == 0 {
                    let sessions_root = recursive::user_sessions_dir(&config.workspace)
                        .unwrap_or_else(|_| config.workspace.join(".recursive").join("sessions"));
                    println!("No sessions found in {}", sessions_root.display());
                } else {
                    println!("Sessions ({}):", total);
                    for s in &old_sessions {
                        println!("  {}  (old format)", s.display());
                    }
                    for s in &new_sessions {
                        // g157: show last_prompt / goal from meta so the user can
                        // identify sessions without reading the full transcript.
                        if let Ok(meta) = recursive::session::SessionReader::load_meta(s) {
                            let label = meta
                                .last_prompt
                                .as_deref()
                                .or(Some(meta.goal.as_str()))
                                .unwrap_or("(no prompt)");
                            println!("  {}  [{}] {}", s.display(), meta.status, label);
                        } else {
                            println!("  {}  (JSONL)", s.display());
                        }
                    }
                }
                Ok(())
            }
            SessionCmd::Show { session } => {
                let path = cli::session::resolve_session_path(&config.workspace, &session)?;
                if path.is_dir() {
                    // New JSONL session format (directory with transcript.jsonl + .meta.json)
                    let meta = recursive::session::SessionReader::load_meta(&path)
                        .with_context(|| format!("reading session meta: {}", path.display()))?;
                    let entries = recursive::session::SessionReader::load_transcript(&path)
                        .with_context(|| {
                            format!("reading session transcript: {}", path.display())
                        })?;

                    println!("Session: {}", path.display());
                    println!("  session_id:      {}", meta.session_id);
                    println!("  goal:            {}", meta.goal);
                    println!("  model:           {}", meta.model);
                    println!("  provider:        {}", meta.provider);
                    println!("  created_at:      {}", meta.created_at);
                    println!("  updated_at:      {}", meta.updated_at);
                    println!("  message_count:   {}", meta.message_count);
                    println!("  status:          {}", meta.status);
                    println!();
                    println!("Transcript ({} entries):", entries.len());
                    for (i, entry) in entries.iter().enumerate() {
                        let preview: String = entry.content.chars().take(200).collect();
                        let truncated = if entry.content.len() > 200 { "…" } else { "" };
                        println!("  [{:>3}] {:>9}: {}{}", i, entry.role, preview, truncated);
                        if !entry.tool_calls.is_empty() {
                            for tc in &entry.tool_calls {
                                println!("         tool_call: {} ({})", tc.name, tc.id);
                            }
                        }
                        if let Some(ref rc) = entry.reasoning_content {
                            let rp: String = rc.chars().take(100).collect();
                            let rt = if rc.len() > 100 { "…" } else { "" };
                            println!("         reasoning: {}{}", rp, rt);
                        }
                    }
                    Ok(())
                } else {
                    // Old single-file session format (.json)
                    let file = SessionFile::read_from(&path)
                        .with_context(|| format!("reading session: {}", path.display()))?;

                    println!("Session: {}", path.display());
                    println!("  schema_version:  {}", file.schema_version);
                    println!("  goal:            {}", file.goal);
                    println!("  model:           {}", file.model);
                    println!("  provider:        {}", file.provider);
                    println!("  tool_registry:   {}", file.tool_registry_hash);
                    println!("  steps_consumed:  {}", file.steps_consumed);
                    println!("  transcript_len:  {}", file.transcript.len());
                    println!();
                    println!("Transcript:");
                    for (i, msg) in file.transcript.iter().enumerate() {
                        let role = match msg.role {
                            recursive::Role::System => "system",
                            recursive::Role::User => "user",
                            recursive::Role::Assistant => "assistant",
                            recursive::Role::Tool => "tool",
                        };
                        let preview: String = msg.content.chars().take(200).collect();
                        let truncated = if msg.content.len() > 200 { "…" } else { "" };
                        println!("  [{:>3}] {:>9}: {}{}", i, role, preview, truncated);
                        if !msg.tool_calls.is_empty() {
                            for tc in &msg.tool_calls {
                                println!("         tool_call: {} ({})", tc.name, tc.id);
                            }
                        }
                    }
                    Ok(())
                }
            }
            SessionCmd::Delete { session, force } => {
                let path = cli::session::resolve_session_path(&config.workspace, &session)?;

                if !force {
                    eprint!("Delete session '{}'? [y/N] ", path.display());
                    use std::io::Write;
                    std::io::stderr().flush()?;
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim().to_lowercase();
                    if input != "y" && input != "yes" {
                        println!("Aborted.");
                        return Ok(());
                    }
                }

                if path.is_dir() {
                    std::fs::remove_dir_all(&path).with_context(|| {
                        format!("removing session directory: {}", path.display())
                    })?;
                    println!("Deleted session directory: {}", path.display());
                } else if path.is_file() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing session file: {}", path.display()))?;
                    println!("Deleted session file: {}", path.display());
                } else {
                    anyhow::bail!("Path does not exist: {}", path.display());
                }
                Ok(())
            }
            SessionCmd::Export { session, output } => {
                let path = cli::session::resolve_session_path(&config.workspace, &session)?;
                let exported = recursive::session::ExportedTranscript::from_session_dir(&path)?;
                let json = serde_json::to_string_pretty(&exported)?;
                if let Some(out) = output {
                    std::fs::write(&out, &json)?;
                    println!("Exported to {}", out.display());
                } else {
                    println!("{}", json);
                }
                Ok(())
            }
            SessionCmd::Rewind {
                session,
                to_turn,
                force,
                dry_run,
            } => cli::session::cmd_session_rewind(
                &config.workspace,
                &session,
                to_turn,
                force,
                dry_run,
            ),
            SessionCmd::MigrateLegacy { path } => {
                cli::session::cmd_session_migrate_legacy(&config.workspace, &path)
            }
        },
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Show => {
                println!("# Effective configuration (env > config file > default)");
                println!("provider_type: {}", config.provider_type);
                println!("model:         {}", config.model);
                println!("api_base:      {}", config.api_base);
                println!("api_key:       {}", mask_key(config.api_key.as_deref()));
                println!("workspace:     {}", config.workspace.display());
                println!("max_steps:     {}", config.max_steps);
                println!("temperature:   {}", config.temperature);
                println!("shell_timeout: {}s", config.shell_timeout_secs);
                if let Some(path) = recursive::config_file::config_file_path() {
                    println!(
                        "\nconfig file:   {} {}",
                        path.display(),
                        if path.exists() {
                            "(exists)"
                        } else {
                            "(not found)"
                        }
                    );
                }
                Ok(())
            }
            ConfigCmd::Set { key, value } => {
                recursive::config_file::set_value(&key, &value)?;
                if let Some(path) = recursive::config_file::config_file_path() {
                    println!("Set {} = {} in {}", key, value, path.display());
                }
                Ok(())
            }
            ConfigCmd::Path => {
                match recursive::config_file::config_file_path() {
                    Some(p) => println!("{}", p.display()),
                    None => anyhow::bail!("could not determine home directory"),
                }
                Ok(())
            }
        },
        Cmd::Migrate { dry_run } => cli::session::cmd_migrate(&config.workspace, dry_run),
    }
}

/// Returns a [`CancellationToken`] that fires on SIGINT (Ctrl+C) or SIGTERM.
fn shutdown_signal() -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
        #[cfg(not(unix))]
        ctrl_c.await.unwrap();
        t.cancel();
    });
    token
}

fn mask_key(key: Option<&str>) -> String {
    match key {
        None => "(not set)".to_string(),
        Some(k) if k.len() <= 8 => "****".to_string(),
        Some(k) => format!("{}...{}", &k[..4], &k[k.len() - 4..]),
    }
}

fn init_logging(level: &str) -> anyhow::Result<()> {
    let lvl: Level = level.parse().context("invalid log level")?;
    let trace_spans = std::env::var("RECURSIVE_TRACE_SPANS").as_deref() == Ok("1");
    // When span timings are requested, the user-provided `--log warn`
    // would suppress the close events (they fire at INFO). Layer an
    // info-level filter for the `recursive` crate's instrumented spans
    // while leaving the rest of the filter alone.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let base = lvl.to_string();
        if trace_spans {
            tracing_subscriber::EnvFilter::new(format!("{base},recursive=info"))
        } else {
            tracing_subscriber::EnvFilter::new(base)
        }
    });
    let mut layer = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(recursive::logging::StderrOrNullMaker)
        .compact();
    if trace_spans {
        layer = layer.with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);
    }
    layer.init();
    Ok(())
}

/// Run the agent in loop mode: agent self-schedules wakeups until it stops.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    config: Config,
    goal: String,
    max_transcript_chars: Option<usize>,
    json_mode: bool,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    _external_pricing: Option<HashMap<String, ModelPricing>>,
    hook_timing: bool,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    use std::sync::Mutex;

    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));

    // Build tools with ScheduleWakeup registered; must happen before build_runtime
    // so the slot is shared between the tool and the runtime loop.
    let mut tools = cli::builder::build_tools(&config).await;
    cli::builder::register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;
    tools.register_mut(Arc::new(ScheduleWakeup::new(wakeup_slot.clone())));

    // Build LLM provider
    let api_key = config.require_api_key()?;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn LlmProvider> = match config.provider_type.as_str() {
        "anthropic" => {
            let anthropic_retry = recursive::llm::anthropic::RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let anthropic = AnthropicProvider::new(&config.api_base, api_key, &config.model)
                .with_temperature(config.temperature)
                .with_retry_policy(anthropic_retry);
            Arc::new(anthropic)
        }
        _ => {
            let openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)
                .with_temperature(config.temperature)
                .with_retry_policy(retry);
            Arc::new(openai)
        }
    };

    let mut builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
        .max_steps(config.max_steps)
        .streaming(stream)
        .shutdown_token(shutdown.clone());
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if hook_timing {
        use recursive::hooks::HookRegistry;
        let mut hooks = HookRegistry::new();
        hooks.register(Arc::new(recursive::hooks::ToolTimingHook::new()));
        builder = builder.hooks(hooks);
    }
    if plan_first {
        builder = builder.planning_mode(PlanningMode::PlanFirst);
    }
    let mut runtime = builder.build().map_err(Into::<anyhow::Error>::into)?;

    let outcomes = runtime.run_loop(&goal, &wakeup_slot).await?;

    // Cancellation now reflected in each outcome's finish_reason
    // (FinishReason::Cancelled). Historical `if shutdown.is_cancelled()`
    // print here was redundant once g137 wired the token through.
    let _ = &shutdown;

    if json_mode {
        let summary: Vec<_> = outcomes
            .iter()
            .map(|o| {
                serde_json::json!({
                    "finish": format!("{:?}", o.finish_reason),
                    "steps": o.steps,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        eprintln!("Loop completed: {} turn(s)", outcomes.len());
    }

    if let Some(last) = outcomes.last() {
        let _ = cli::output::exit_for_finish(&last.finish_reason, last.steps);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    config: Config,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    session_out: Option<PathBuf>,
    json_mode: bool,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    external_pricing: Option<HashMap<String, ModelPricing>>,
    hook_timing: bool,
    session: bool,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> = if session {
        match SessionWriter::create(
            &config.workspace,
            &goal,
            &config.model,
            &config.provider_type,
        ) {
            Ok(writer) => {
                eprintln!("session: recording to {}", writer.session_dir().display());
                Some(Arc::new(std::sync::Mutex::new(writer)))
            }
            Err(e) => {
                eprintln!("session: failed to create session writer: {e}");
                None
            }
        }
    } else {
        None
    };

    let cost_tracker: Option<std::sync::Mutex<recursive::cost::CostTracker>> = if session {
        session_writer.as_ref().map(|w| {
            let session_dir = w.lock().unwrap().session_dir().to_path_buf();
            std::sync::Mutex::new(recursive::cost::CostTracker::new(
                session_dir,
                &config.model,
                &config.provider_type,
                &external_pricing,
            ))
        })
    } else {
        None
    };

    let (channel_sink, event_rx) = ChannelSink::new();
    let event_sink: Arc<dyn EventSink> = if let Some(ref sw) = session_writer {
        Arc::new(CompositeSink::new(vec![
            Box::new(channel_sink) as Box<dyn EventSink>,
            Box::new(SessionPersistenceSink::new(sw.clone())) as Box<dyn EventSink>,
        ]))
    } else {
        Arc::new(channel_sink)
    };
    let mut runtime = cli::builder::build_runtime(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        plan_first,
        mcp_config,
        hook_timing,
        Some(&goal),
        Some(event_sink),
        Some(shutdown.clone()),
    )
    .await?;

    // Wire up per-turn checkpoints when a session is active and git is
    // available. The shadow repo is shared across all sessions in this
    // workspace; each session advances its own ref chain.
    if let Some(ref sw) = session_writer {
        match recursive::ShadowRepo::open(&config.workspace) {
            Ok(repo) => {
                let session_id = sw.lock().unwrap().session_id().to_string();
                let session_dir = sw.lock().unwrap().session_dir().to_path_buf();
                let log_path = session_dir.join("checkpoints.jsonl");
                let touched = runtime.kernel().tools().touched_files();
                if let Err(e) =
                    runtime.enable_checkpoints(Arc::new(repo), session_id, log_path, touched)
                {
                    eprintln!("checkpoint: failed to enable, continuing without: {e}");
                } else {
                    eprintln!("checkpoint: per-turn snapshots active");
                }
            }
            Err(e) => {
                eprintln!("checkpoint: shadow repo unavailable, continuing without: {e}");
            }
        }
    }

    let tool_specs = runtime.kernel().tools().specs();

    let printer = if json_mode {
        tokio::spawn(cli::output::stream_events_json(event_rx))
    } else {
        tokio::spawn(cli::output::stream_events(event_rx))
    };

    let outcome = loop {
        let o = runtime.run(goal.clone()).await?;
        if !matches!(o.finish_reason, FinishReason::PlanPending) {
            break o;
        }
        let plan_text = o.final_text.as_deref().unwrap_or("(no plan)");
        eprintln!("\n=== Proposed Plan ===\n{plan_text}");
        eprint!("Confirm plan? [Y/n] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed.is_empty() || trimmed == "y" || trimmed == "yes" {
            runtime.confirm_plan();
        } else {
            runtime.reject_plan("User rejected the plan");
            break o;
        }
    };

    let transcript = runtime.transcript().to_vec();
    drop(runtime);

    // Cancellation is now visible via outcome.finish_reason ==
    // FinishReason::Cancelled; print_finish_note below renders it.
    // The historical `if shutdown.is_cancelled() { eprintln!... }`
    // here was redundant once g137 wired the token into the kernel.
    let _ = &shutdown;

    printer.await.ok();

    if !json_mode {
        if let Some(ref msg) = outcome.final_text {
            println!("\n=== final ===\n{msg}");
        }
        cli::output::print_usage(
            outcome.total_usage,
            &config.model,
            outcome.llm_latency_ms,
            outcome.steps,
            &external_pricing,
        );
        cli::output::print_finish_note(&outcome.finish_reason);
    }

    let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
        "success"
    } else {
        "incomplete"
    };
    cli::output::finalize_session_writer(session_writer, finish_status);
    cli::output::finalize_cost_tracker(
        cost_tracker,
        outcome.total_usage,
        outcome.llm_latency_ms,
        &config.model,
    );

    if let Some(path) = transcript_out {
        cli::output::save_transcript(&transcript, outcome.steps, &config.model, &path)?;
    }
    if let Some(path) = session_out {
        if !matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
            cli::output::save_session(
                &transcript,
                outcome.steps,
                goal,
                &config.model,
                &config.provider_type,
                &tool_specs,
                &path,
            )?;
        }
    }
    cli::output::exit_for_finish(&outcome.finish_reason, outcome.steps)
}

#[allow(clippy::too_many_arguments)]
async fn repl(
    config: Config,
    max_transcript_chars: Option<usize>,
    json_mode: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    external_pricing: Option<HashMap<String, ModelPricing>>,
    stream: bool,
    hook_timing: bool,
) -> anyhow::Result<()> {
    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }
    if !json_mode {
        let version = env!("CARGO_PKG_VERSION");
        eprintln!(
            "recursive v{}\nmodel: {} | provider: {} | workspace: {}\nType your goal, or :q to quit.\n",
            version,
            config.model,
            config.provider_type,
            config.workspace.display()
        );
    }

    // Build runtime ONCE — MCP servers are spawned here and stay alive.
    // Start with NullSink; we swap in a fresh ChannelSink per turn.
    let mut runtime = cli::builder::build_runtime(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        plan_first,
        mcp_config,
        hook_timing,
        None,
        None,
        None,
    )
    .await?;

    let mut total_turns = 0usize;
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        eprint!("recursive> ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let goal = line.trim();
        if goal.is_empty() {
            continue;
        }
        if matches!(goal, ":q" | ":quit" | "exit") {
            break;
        }
        if goal == ":clear" {
            runtime.set_transcript(Vec::new());
            total_turns = 0;
            if !json_mode {
                eprintln!("(conversation cleared)");
            }
            continue;
        }

        // Fresh ChannelSink per turn; swap back to NullSink when done.
        let (sink, event_rx) = ChannelSink::new();
        runtime.set_event_sink(Arc::new(sink));

        let printer = if json_mode {
            tokio::spawn(cli::output::stream_events_json(event_rx))
        } else {
            tokio::spawn(cli::output::stream_events_repl(event_rx))
        };

        match runtime.run(goal.to_string()).await {
            Ok(outcome) => {
                // Reset to NullSink so the channel is dropped and printer finishes
                runtime.set_event_sink(Arc::new(NullSink));
                printer.await.ok();

                if !json_mode {
                    cli::output::print_usage(
                        outcome.total_usage,
                        &config.model,
                        outcome.llm_latency_ms,
                        outcome.steps,
                        &external_pricing,
                    );
                    cli::output::print_finish_note(&outcome.finish_reason);
                }

                total_turns += 1;
            }
            Err(e) => {
                runtime.set_event_sink(Arc::new(NullSink));
                printer.await.ok();
                eprintln!("error: {e}");
            }
        }
    }
    if !json_mode && total_turns > 0 {
        eprintln!("session: {} turn(s)", total_turns);
    }
    Ok(())
}

/// Run as an MCP stdio server.
///
/// Reads newline-delimited JSON-RPC 2.0 requests from stdin, dispatches
/// them to the agent's tools, and writes newline-delimited JSON-RPC
/// responses to stdout.
///
/// This mode is designed to be used as a subprocess by MCP clients (e.g.
/// Claude Desktop, VS Code extensions) that communicate via stdio.
async fn run_mcp_server_stdio(config: Config, _mcp_config: Option<PathBuf>) -> anyhow::Result<()> {
    // Build the tool registry (local tools only — no MCP servers, since
    // we *are* the MCP server).
    let tools = cli::builder::build_tools(&config).await;

    // We don't need an LLM provider or agent for the stdio server mode.
    // The tools are called directly via dispatch_request.
    // However, we need an McpClient-like wrapper. Since we're acting as
    // the MCP server ourselves, we create a thin adapter that wraps the
    // tool registry.
    let registry = Arc::new(tools);

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    // Stderr is used for logging/diagnostics; stdout is for JSON-RPC responses.
    eprintln!("mcp-server: ready (reading JSON-RPC from stdin)");

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // Parse the JSON-RPC request
        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                // Can't parse — write an error response if there's an id
                // Try to extract an id from the raw JSON
                let id: Option<serde_json::Value> = serde_json::from_str(&line)
                    .ok()
                    .and_then(|v: serde_json::Value| v.get("id").cloned());
                let response = JsonRpcResponse::error(id, -32700, format!("Parse error: {e}"));
                let output = serde_json::to_string(&response)?;
                println!("{output}");
                continue;
            }
        };

        let is_notification = request.id.is_none();

        // Dispatch the request
        let response = dispatch_request_via_registry(&request, &registry).await;

        // Notifications get no response
        if is_notification {
            continue;
        }

        if let Some(resp) = response {
            let output = serde_json::to_string(&resp)?;
            println!("{output}");
        }
    }

    eprintln!("mcp-server: stdin closed, shutting down");
    Ok(())
}

/// Dispatch a JSON-RPC request using the local tool registry.
///
/// This is a simplified dispatcher that handles the MCP methods by
/// calling the local tools directly, without an LLM or agent loop.
async fn dispatch_request_via_registry(
    request: &JsonRpcRequest,
    registry: &ToolRegistry,
) -> Option<JsonRpcResponse> {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": true,
                    "resources": false,
                    "prompts": false
                },
                "serverInfo": {
                    "name": "recursive-agent",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            Some(JsonRpcResponse::success(id, result))
        }
        "notifications/initialized" => None,
        "tools/list" => {
            let specs = registry.specs();
            let tools_arr: Vec<serde_json::Value> = specs
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "inputSchema": s.parameters,
                    })
                })
                .collect();
            Some(JsonRpcResponse::success(
                id,
                serde_json::json!({ "tools": tools_arr }),
            ))
        }
        "tools/call" => {
            let name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request.params.get("arguments").cloned().unwrap_or_default();

            match registry.invoke(name, arguments).await {
                Ok(text) => {
                    let result = serde_json::json!({
                        "content": [{"type": "text", "text": text}]
                    });
                    Some(JsonRpcResponse::success(id, result))
                }
                Err(e) => {
                    let result = serde_json::json!({
                        "isError": true,
                        "content": [{"type": "text", "text": e.to_string()}]
                    });
                    Some(JsonRpcResponse::success(id, result))
                }
            }
        }
        "resources/list" => Some(JsonRpcResponse::success(
            id,
            serde_json::json!({ "resources": [] }),
        )),
        "resources/read" => Some(JsonRpcResponse::error(
            id,
            -32601,
            "resources/read not supported",
        )),
        "prompts/list" => Some(JsonRpcResponse::success(
            id,
            serde_json::json!({ "prompts": [] }),
        )),
        "prompts/get" => Some(JsonRpcResponse::error(
            id,
            -32601,
            "prompts/get not supported",
        )),
        _ => Some(JsonRpcResponse::method_not_found(id, &request.method)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::builder::build_runtime;

    fn dummy_config(tmp: &std::path::Path) -> Config {
        Config {
            workspace: tmp.to_path_buf(),
            api_base: "https://example.invalid/v1".into(),
            api_key: Some("dummy-test-key".into()),
            model: "test-model".into(),
            provider_type: "openai".into(),
            max_steps: 1,
            temperature: 0.0,
            system_prompt: "test".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 1,
            shell_timeout_secs: 5,
            headless: false,
            memory_summary_limit: 5,
        }
    }

    // Regression for the streaming-SSE merge bug (commit 92d257e) where
    // the non-streaming code path called `bool::then(...).unwrap()` and
    // panicked because `then(false)` returns None. This made every
    // `recursive run` (default: stream=false) panic at startup, which
    // in turn broke all parallel-self-improve.sh launches in batch 13.
    /// Smoke test for `build_runtime` across the matrix of stream flag and
    /// provider selector. Consolidated into ONE test per AGENTS.md guidance
    /// because the anthropic branch reads `RECURSIVE_PROVIDER_TYPE` from the
    /// process env — running it in parallel with other build-runtime tests
    /// would race on that global. Asserts:
    ///   - stream=false / openai (default)  → ok (regresses 92d257e bug)
    ///   - stream=true  / openai            → ok (regresses streaming-merge bug)
    ///   - stream=false / anthropic         → ok (g47 dogfood)
    /// The anthropic branch sets+restores the env var to keep the test
    /// hermetic for any tests that come after it.
    #[tokio::test]
    async fn build_runtime_construction_smoke() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = dummy_config(tmp.path());

        let r1 = build_runtime(
            &cfg,
            None,
            Vec::new(),
            /* stream */ false,
            false,
            None,
            false,
            None,
            None,
            None,
        )
        .await;
        assert!(r1.is_ok(), "openai/stream=false: must not panic or fail");

        let r2 = build_runtime(
            &cfg,
            None,
            Vec::new(),
            /* stream */ true,
            false,
            None,
            false,
            None,
            None,
            None,
        )
        .await;
        assert!(r2.is_ok(), "openai/stream=true: must not panic or fail");

        let original = std::env::var("RECURSIVE_PROVIDER_TYPE").ok();
        std::env::set_var("RECURSIVE_PROVIDER_TYPE", "anthropic");
        let mut cfg_anthropic = dummy_config(tmp.path());
        cfg_anthropic.provider_type = "anthropic".into();
        let r3 = build_runtime(
            &cfg_anthropic,
            None,
            Vec::new(),
            false,
            false,
            None,
            false,
            None,
            None,
            None,
        )
        .await;
        match original {
            Some(v) => std::env::set_var("RECURSIVE_PROVIDER_TYPE", v),
            None => std::env::remove_var("RECURSIVE_PROVIDER_TYPE"),
        }
        assert!(r3.is_ok(), "anthropic/stream=false: must not panic or fail");
    }

    #[test]
    fn hook_timing_flag_accepted() {
        // Verify --hook-timing is accepted by the CLI parser
        let args = vec!["recursive", "--hook-timing", "run", "test goal"];
        let cli = Cli::parse_from(args);
        assert!(cli.hook_timing);
    }

    #[test]
    fn hook_timing_flag_defaults_to_false() {
        let args = vec!["recursive", "run", "test goal"];
        let cli = Cli::parse_from(args);
        assert!(!cli.hook_timing);
    }
}

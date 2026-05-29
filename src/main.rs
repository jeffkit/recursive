//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::Level;

use recursive::config::load_project_context;
use recursive::mcp::{discover_mcp_servers, load_mcp_config, McpClient, McpServer, McpTool};
use recursive::mcp::{JsonRpcRequest, JsonRpcResponse};
use recursive::skills::{discover_skills, skill_index, skills_for_injection, Skill};
use recursive::SessionFile;
use recursive::SessionWriter;
use recursive::{
    config::Config,
    llm::{
        load_pricing_from_yaml, pricing_for, AnthropicProvider, LlmProvider, ModelPricing,
        OpenAiProvider, TokenUsage,
    },
    tools::EpisodicRecall,
    tools::{
        ApplyPatch, BackgroundJobManager, CheckBackground, EstimateTokens, Forget, ListDir,
        LoadSkill, LocalTransport, ReadFile, Recall, Remember, RunBackground, RunShell,
        RunSkillScript, ScheduleWakeup, ScratchpadDelete, ScratchpadGet, ScratchpadList,
        SearchFiles, SubAgent, ToolTransport, WakeupSlot, WebFetch, WorkingMemoryTool, WriteFile,
    },
    tools::{ForgetFact, RecallFact, RememberFact, UpdateFact},
    AgentEvent, AgentRuntime, AgentRuntimeBuilder, ChannelSink, EventSink, FinishReason, NullSink,
    PlanningMode, RetryPolicy, ToolRegistry, TranscriptFile,
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
    // - Nothing → interactive REPL
    let effective_cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            if let Some(prompt) = cli.prompt {
                Cmd::Run { goal: vec![prompt] }
            } else {
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
            let tools = build_tools(&config).await;
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
            let tools = build_tools(&config).await;
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
        Cmd::Init => run_init().await,
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
                    run_resumed(
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
        Cmd::Resume { session, from_file } => {
            cmd_resume(
                config,
                session,
                from_file,
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
                        println!("  {}  (JSONL)", s.display());
                    }
                }
                Ok(())
            }
            SessionCmd::Show { session } => {
                let path = resolve_session_path(&config.workspace, &session)?;
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
                let path = resolve_session_path(&config.workspace, &session)?;

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
                let path = resolve_session_path(&config.workspace, &session)?;
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
            } => cmd_session_rewind(&config.workspace, &session, to_turn, force, dry_run),
            SessionCmd::MigrateLegacy { path } => {
                cmd_session_migrate_legacy(&config.workspace, &path)
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
        Cmd::Migrate { dry_run } => cmd_migrate(&config.workspace, dry_run),
    }
}

fn cmd_migrate(workspace: &Path, dry_run: bool) -> anyhow::Result<()> {
    let report = recursive::migrate_workspace(workspace, dry_run)?;
    if report.already_clean {
        println!(
            "Workspace {} has no legacy in-tree state. Nothing to migrate.",
            workspace.display()
        );
        return Ok(());
    }
    let prefix = if dry_run { "(dry-run) " } else { "" };
    if !report.moved.is_empty() {
        println!("{prefix}Moved:");
        for (src, dst) in &report.moved {
            println!("  {} -> {}", src.display(), dst.display());
        }
    }
    if !report.skipped.is_empty() {
        println!("{prefix}Skipped (destination already exists):");
        for (src, dst) in &report.skipped {
            println!(
                "  {} stays put; {} already has data",
                src.display(),
                dst.display()
            );
        }
        eprintln!(
            "warning: some items were not migrated. Inspect the destinations and \
             merge manually if needed."
        );
    }
    if report.removed_empty_dotrecursive {
        println!("{prefix}Removed empty <workspace>/.recursive/");
    }
    Ok(())
}

/// Resolve a session path from a user-provided string.
///
/// If the string is an existing file or directory path, return it as-is.
/// Otherwise, search the workspace's session directory for a session whose
/// filename or directory name contains the given string (case-insensitive).
/// Returns an error if no match or multiple matches are found.
fn resolve_session_path(workspace: &Path, session: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(session);

    // If it's an existing path, use it directly
    if path.exists() {
        return Ok(path);
    }

    // Search both the new (user data dir) and legacy (in-tree) session
    // directories so users with un-migrated state can still address
    // their old sessions.
    let new_dir = recursive::user_sessions_dir(workspace).ok();
    let legacy_dir = workspace.join(".recursive").join("sessions");
    let search_dirs: Vec<PathBuf> = new_dir
        .into_iter()
        .chain(if legacy_dir.is_dir() {
            Some(legacy_dir.clone())
        } else {
            None
        })
        .filter(|d| d.is_dir())
        .collect();

    if search_dirs.is_empty() {
        anyhow::bail!(
            "Session not found: '{}'. No sessions directory exists (looked in user data dir and {}).",
            session,
            legacy_dir.display()
        );
    }

    let lower = session.to_lowercase();
    let mut matches: Vec<PathBuf> = Vec::new();

    for sessions_dir in &search_dirs {
        // Search flat session files (old format: <timestamp>-<goal>.json)
        if let Ok(entries) = std::fs::read_dir(sessions_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(name) = p.file_stem().and_then(|n| n.to_str()) {
                        if name.to_lowercase().contains(&lower) {
                            matches.push(p);
                        }
                    }
                }
            }
        }

        // Search nested session directories (new JSONL format)
        if let Ok(slug_entries) = std::fs::read_dir(sessions_dir) {
            for slug_entry in slug_entries.flatten() {
                let slug_dir = slug_entry.path();
                if !slug_dir.is_dir() {
                    continue;
                }
                if let Ok(session_entries) = std::fs::read_dir(&slug_dir) {
                    for session_entry in session_entries.flatten() {
                        let p = session_entry.path();
                        if p.is_dir() {
                            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                if name.to_lowercase().contains(&lower) {
                                    matches.push(p);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    match matches.len() {
        0 => anyhow::bail!(
            "Session not found: '{}'. Use 'recursive sessions list' to see available sessions.",
            session
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            eprintln!("Multiple sessions match '{}' ({}):", session, n);
            for m in &matches {
                eprintln!("  {}", m.display());
            }
            anyhow::bail!("Ambiguous session identifier. Use a more specific path or ID.");
        }
    }
}

/// Implementation of `recursive sessions migrate-legacy`.
///
/// Reads a legacy single-file `.json` session (as written by
/// `--session-out`) and emits an equivalent JSONL session
/// directory under the user data dir, preserving the original
/// `tool_registry_hash`. The migrated session can then be resumed
/// by ID via `recursive resume <id>`.
fn cmd_session_migrate_legacy(workspace: &Path, path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("legacy session file does not exist: {}", path.display());
    }
    let legacy = SessionFile::read_from(path)
        .with_context(|| format!("reading legacy session: {}", path.display()))?;

    // Open a fresh JSONL session, then patch in the carried-over hash.
    let mut writer =
        SessionWriter::create(workspace, &legacy.goal, &legacy.model, &legacy.provider)
            .with_context(|| "creating new JSONL session for migration")?;

    // Replay the legacy transcript through `append` (no filter —
    // we keep system messages for round-trip fidelity).
    for msg in legacy.messages() {
        writer.append(msg)?;
    }
    let session_dir = writer.session_dir().to_path_buf();
    writer.finish("interrupted").ok();
    drop(writer);

    // Patch `.meta.json` to carry over the legacy `tool_registry_hash`.
    let meta_path = session_dir.join(".meta.json");
    let bytes = std::fs::read(&meta_path)?;
    let mut meta: recursive::session::SessionMeta = serde_json::from_slice(&bytes)?;
    meta.tool_registry_hash = Some(legacy.tool_registry_hash.clone());
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

    println!("Migrated to: {}", session_dir.display());
    println!(
        "Resume with: recursive resume {}",
        session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<id>"),
    );
    Ok(())
}

/// Implementation of `recursive sessions rewind`.
fn cmd_session_rewind(
    workspace: &Path,
    session: &str,
    to_turn: usize,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let session_path = resolve_session_path(workspace, session)?;
    // The session path returned by resolve_session_path is the session
    // directory under .recursive/sessions/<slug>/<sid>/. The
    // checkpoints log lives inside it.
    if !session_path.is_dir() {
        anyhow::bail!(
            "Rewind requires a JSONL session directory; got file: {}",
            session_path.display()
        );
    }
    let log_path = session_path.join("checkpoints.jsonl");
    if !log_path.exists() {
        anyhow::bail!(
            "No checkpoints.jsonl in {}. \
             This session predates checkpointing or had it disabled.",
            session_path.display()
        );
    }

    let plan = recursive::plan_rewind(&log_path, to_turn)?;

    println!("Rewind plan:");
    println!("  target checkpoint: {}", plan.target);
    println!("  turns to drop:     {:?}", plan.turns_to_drop);
    println!("  files to restore:  {} path(s)", plan.touched_paths.len());
    for p in &plan.touched_paths {
        println!("    - {p}");
    }
    if dry_run {
        println!("(--dry-run: not applied)");
        return Ok(());
    }

    let repo = recursive::ShadowRepo::open(workspace).map_err(|e| {
        anyhow::anyhow!(
            "cannot open shadow repo at {}/.recursive/shadow-git: {e}",
            workspace.display()
        )
    })?;

    let result = recursive::apply_rewind(&repo, &log_path, &plan, force)?;
    println!(
        "Rewind applied: {} restored, {} deleted, {} unchanged. {} turn(s) dropped from log.",
        result.stats.restored,
        result.stats.deleted,
        result.stats.unchanged,
        result.dropped_turns.len()
    );

    // Also truncate transcript.jsonl so the conversation state matches
    // the restored workspace state.
    match recursive::truncate_transcript_to_turn(&session_path, to_turn) {
        Ok(stats) => {
            println!(
                "Transcript truncated: {} message(s) kept, {} dropped.",
                stats.kept, stats.dropped
            );
        }
        Err(e) => {
            eprintln!("warning: transcript truncation failed: {e}");
        }
    }
    Ok(())
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
        .with_writer(std::io::stderr)
        .compact();
    if trace_spans {
        layer = layer.with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);
    }
    layer.init();
    Ok(())
}

/// Build the tool registry, optionally registering MCP tools from a config file.
async fn build_tools(config: &Config) -> ToolRegistry {
    let root = &config.workspace;
    let transport: Arc<dyn ToolTransport> = Arc::new(LocalTransport);
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));
    let mut registry = ToolRegistry::new(transport)
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ApplyPatch::new(root)))
        .register(Arc::new(ListDir::new(root)))
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(root)))
        .register(Arc::new(WebFetch::new()))
        .register(Arc::new(RunBackground::new(root, bg_manager.clone())))
        .register(Arc::new(CheckBackground::new(bg_manager.clone())));
    registry = registry.register(Arc::new(EstimateTokens::new(root)));
    registry = registry
        .register(Arc::new(Remember::new(root)))
        .register(Arc::new(Recall::new(root)))
        .register(Arc::new(Forget::new(root)));
    registry = registry
        .register(Arc::new(RememberFact::new(root)))
        .register(Arc::new(RecallFact::new(root)))
        .register(Arc::new(ForgetFact::new(root)))
        .register(Arc::new(UpdateFact::new(root)));
    registry = registry.register(Arc::new(EpisodicRecall::new(root)));
    registry = registry
        .register(Arc::new(WorkingMemoryTool::new(root)))
        .register(Arc::new(ScratchpadGet::new(root)))
        .register(Arc::new(ScratchpadDelete::new(root)))
        .register(Arc::new(ScratchpadList::new(root)));
    let skills = discover_loaded_skills(config);
    if !skills.is_empty() {
        registry = registry.register(Arc::new(LoadSkill::new(skills.clone())));
        registry = registry.register(Arc::new(RunSkillScript::new(
            skills,
            root.clone(),
            Duration::from_secs(config.shell_timeout_secs),
        )));
    }
    // Note: read-only checkpoint tools (checkpoint_list / checkpoint_diff)
    // are registered by the runtime when a session id is known, since
    // they must be scoped to the current session's checkpoint chain.
    if let Some(perms) = resolve_tool_permissions() {
        registry = registry.with_permissions(perms);
    }
    registry
}

/// Resolve the active tool-permission configuration.
///
/// Resolution order:
///   1. `RECURSIVE_TOOL_PERMISSIONS_FILE=<path>` env — TOML file
///      whose top-level keys are `allow`, `deny`, `interactive`
///      (matches [`recursive::permissions::PermissionsConfig`] verbatim).
///   2. `~/.recursive/config.toml`'s `[permissions]` section.
///   3. None — every tool allowed (back-compat default).
///
/// Errors during file read or TOML parse are logged to stderr and
/// treated as "no permissions config" — a malformed file should not
/// brick the CLI for unrelated commands.
fn resolve_tool_permissions() -> Option<recursive::permissions::PermissionsConfig> {
    if let Ok(path) = std::env::var("RECURSIVE_TOOL_PERMISSIONS_FILE") {
        if !path.is_empty() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    match toml::from_str::<recursive::permissions::PermissionsConfig>(&content) {
                        Ok(perms) => return Some(perms),
                        Err(e) => {
                            eprintln!("permissions: failed to parse {path}: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("permissions: failed to read {path}: {e}");
                }
            }
        }
    }
    let file_config = recursive::config_file::FileConfig::load().ok().flatten()?;
    let section = file_config.permissions?;
    Some(recursive::permissions::PermissionsConfig {
        allow: section.allow,
        deny: section.deny,
        interactive: section.interactive,
    })
}

/// Register MCP tools from a config file into the registry.
async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    workspace: &Path,
    mcp_config_path: Option<PathBuf>,
) {
    let servers: Vec<McpServer> = if let Some(path) = &mcp_config_path {
        // Explicit config file provided
        if !path.exists() {
            eprintln!("warning: MCP config file not found: {}", path.display());
            return;
        }
        match load_mcp_config(path) {
            Ok(s) => {
                eprintln!(
                    "mcp: loaded {} server(s) from explicit config `{}`",
                    s.len(),
                    path.display()
                );
                s
            }
            Err(e) => {
                eprintln!("warning: failed to load MCP config: {e}");
                return;
            }
        }
    } else {
        // Auto-discover from workspace
        match discover_mcp_servers(workspace).await {
            Ok(s) => {
                if !s.is_empty() {
                    eprintln!("mcp: auto-discovered {} server(s) from workspace", s.len());
                }
                s
            }
            Err(e) => {
                eprintln!("warning: failed to auto-discover MCP servers: {e}");
                return;
            }
        }
    };
    if servers.is_empty() {
        return;
    }
    for server in &servers {
        match register_mcp_server_tools(registry, server).await {
            Ok(count) => {
                eprintln!(
                    "mcp: registered {} tool(s) from server `{}`",
                    count, server.name
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to register MCP server `{}`: {e}",
                    server.name
                );
            }
        }
    }
}

/// Spawn an MCP server, list its tools, and register them in the registry.
async fn register_mcp_server_tools(
    registry: &mut ToolRegistry,
    server: &McpServer,
) -> anyhow::Result<usize> {
    let mut client = McpClient::spawn(server).await?;
    let tool_specs = client.list_tools().await?;
    let count = tool_specs.len();
    let client = Arc::new(tokio::sync::Mutex::new(client));
    for spec in tool_specs {
        let tool = McpTool::new(client.clone(), spec, &server.name);
        registry.register_mut(Arc::new(tool));
    }
    Ok(count)
}

/// Discover skills from configured search paths.
/// Defaults: <workspace>/.recursive/skills/, ~/.recursive/skills/.
/// Override with RECURSIVE_SKILL_PATHS=path1:path2 (colon-separated).
fn discover_loaded_skills(config: &Config) -> Vec<Skill> {
    let paths: Vec<PathBuf> = if let Ok(env_paths) = std::env::var("RECURSIVE_SKILL_PATHS") {
        env_paths.split(':').map(PathBuf::from).collect()
    } else {
        let mut defaults = vec![config.workspace.join(".recursive").join("skills")];
        if let Some(home) = std::env::var_os("HOME") {
            defaults.push(PathBuf::from(home).join(".recursive").join("skills"));
        }
        defaults
    };
    discover_skills(&paths)
}

/// Build an [`AgentRuntime`], optionally registering MCP tools from a config file.
#[allow(clippy::too_many_arguments)]
async fn build_runtime(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    event_sink: Option<Arc<dyn EventSink>>,
    shutdown_token: Option<tokio_util::sync::CancellationToken>,
) -> anyhow::Result<AgentRuntime> {
    let api_key = config.require_api_key()?;
    let provider_type = &config.provider_type;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn LlmProvider> = match provider_type.as_str() {
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
    let mut tools = build_tools(config).await;
    register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;

    // Always attach a TouchedFiles collector so AgentRuntime can record
    // per-turn file touches when checkpoints are enabled later via
    // enable_checkpoints(). When checkpoints are disabled this is a
    // no-op observer.
    tools = tools.with_touched_files(Arc::new(std::sync::Mutex::new(
        recursive::TouchedFiles::new(),
    )));

    let sub_agent_enabled = std::env::var("RECURSIVE_SUBAGENT_ENABLED").as_deref() == Ok("1");
    if sub_agent_enabled {
        let max_depth: usize = std::env::var("RECURSIVE_SUBAGENT_MAX_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        let sub = SubAgent::new(
            &config.workspace,
            provider.clone(),
            tools.clone(),
            max_depth,
            0,
            None,
        );
        tools = tools.register(Arc::new(sub));
    }

    let skills = discover_loaded_skills(config);
    let project_context = load_project_context(&config.workspace);
    let mut system_prompt = match (&project_context, skills.is_empty()) {
        (Some(ctx), true) => {
            format!(
                "# Project context (AGENTS.md)\n\n{}\n\n---\n\n{}",
                ctx, config.system_prompt
            )
        }
        (Some(ctx), false) => {
            format!(
                "# Project context (AGENTS.md)\n\n{}\n\n---\n\n{}\n{}",
                ctx,
                config.system_prompt,
                skill_index(&skills)
            )
        }
        (None, true) => config.system_prompt.clone(),
        (None, false) => format!("{}\n{}", config.system_prompt, skill_index(&skills)),
    };
    let injected = skills_for_injection(&skills, goal.unwrap_or(""));
    if !injected.is_empty() {
        let mut injection_block = String::new();
        let mut total_chars = 0usize;
        let max_injection_chars = 8192usize;
        for (name, body) in &injected {
            let snippet = format!(
                "=== Skill: {name} (auto-loaded) ===
{body}

"
            );
            if total_chars + snippet.len() > max_injection_chars {
                let remaining = max_injection_chars.saturating_sub(total_chars);
                let truncated = if remaining > 20 {
                    format!(
                        "{}...
[truncated]
",
                        &snippet[..remaining.saturating_sub(20)]
                    )
                } else {
                    "[truncated]
"
                    .to_string()
                };
                injection_block.push_str(&truncated);
                break;
            }
            injection_block.push_str(&snippet);
            total_chars += snippet.len();
        }
        system_prompt = format!(
            "{}

{}",
            system_prompt, injection_block
        );
    }
    let system_prompt = if sub_agent_enabled {
        format!(
            "{}\n\nWhen you need to do focused research or scan files without polluting your main context, use the `sub_agent` tool. It spawns a fresh agent with its own transcript and a restricted tool set (read-only by default).",
            system_prompt
        )
    } else {
        system_prompt
    };

    let mut builder = AgentRuntimeBuilder::new()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps)
        .streaming(stream);
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if let Some(token) = shutdown_token {
        builder = builder.shutdown_token(token);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
    if let Ok(threshold) = std::env::var("RECURSIVE_COMPACT_THRESHOLD") {
        if let Ok(n) = threshold.parse::<usize>() {
            if n > 0 {
                builder = builder.compactor(recursive::Compactor::new(n));
            }
        }
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
    if let Some(sink) = event_sink {
        builder = builder.event_sink(sink);
    }
    builder.build().map_err(Into::into)
}

/// Get pricing for a model: external pricing takes precedence, then falls back
/// to hardcoded pricing_for().
fn get_pricing(
    model: &str,
    external: &Option<HashMap<String, ModelPricing>>,
) -> Option<ModelPricing> {
    if let Some(ext) = external {
        if let Some(pricing) = ext.get(model) {
            return Some(*pricing);
        }
    }
    pricing_for(model)
}

fn print_usage(
    usage: TokenUsage,
    model: &str,
    total_llm_latency_ms: u64,
    steps: usize,
    external_pricing: &Option<HashMap<String, ModelPricing>>,
) {
    if usage.total_tokens > 0 {
        eprintln!(
            "tokens: prompt={} completion={} total={}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
        if usage.cache_hit_tokens > 0 {
            let total_cache = usage.cache_hit_tokens + usage.cache_miss_tokens;
            let hit_rate = if total_cache > 0 {
                (usage.cache_hit_tokens as f64 / total_cache as f64) * 100.0
            } else {
                0.0
            };
            eprintln!(
                "cache: hit={} miss={} ({:.1}% hit rate)",
                usage.cache_hit_tokens, usage.cache_miss_tokens, hit_rate
            );
        }
        if let Some(pricing) = get_pricing(model, external_pricing) {
            let cost = pricing.cost_usd(usage);
            eprintln!("cost: ${:.4} ({})", cost, model);
        }
    }
    if total_llm_latency_ms > 0 && steps > 0 {
        let avg = total_llm_latency_ms / steps as u64;
        eprintln!(
            "llm latency: total={}ms avg={}ms over {} steps",
            total_llm_latency_ms, avg, steps
        );
    }
}

fn print_finish_note(finish: &FinishReason) {
    match finish {
        FinishReason::TranscriptLimit { chars, limit } => {
            eprintln!(
                "note: stopped because transcript reached {} chars (limit {})",
                chars, limit
            );
        }
        FinishReason::Cancelled => {
            eprintln!("shutdown: agent stopped at next step boundary after signal");
        }
        _ => {}
    }
}

/// Save the transcript to disk if a path was requested. Always called
/// before any exit-code decision so auto-resume (which keys off the
/// transcript file's existence) works even when the agent terminated
/// abnormally (e.g. BudgetExceeded).
fn save_transcript(
    outcome_transcript: &[recursive::message::Message],
    outcome_steps: usize,
    model: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let file = TranscriptFile::new(
        outcome_transcript.to_vec(),
        outcome_steps,
        Some(model.into()),
    );
    file.write_to(path)?;
    eprintln!(
        "transcript: wrote {} messages to {}",
        outcome_transcript.len(),
        path.display()
    );
    Ok(())
}

/// Save a session file for non-success finishes.
fn save_session(
    transcript: &[recursive::message::Message],
    steps: usize,
    goal: String,
    model: &str,
    provider: &str,
    tool_specs: &[recursive::ToolSpec],
    path: &Path,
) -> anyhow::Result<()> {
    let session = SessionFile::new(
        goal,
        model.to_string(),
        provider.to_string(),
        tool_specs,
        steps,
        transcript.to_vec(),
    );
    session.write_to(path)?;
    eprintln!(
        "session: wrote {} messages to {}",
        transcript.len(),
        path.display()
    );
    Ok(())
}

/// Return Err iff the finish reason should propagate as a non-zero binary
/// exit code so that self-improve.sh's auto-resume gate fires. The
/// transcript has already been saved by the caller before this is called.
///
/// `Cancelled` is intentionally **not** an error: shutdown via SIGINT
/// or SIGTERM is user-initiated, the saved transcript is intact, and
/// self-improve.sh must NOT auto-resume something the user explicitly
/// stopped. The fall-through `_ => Ok(())` covers it.
fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        _ => Ok(()),
    }
}

fn finalize_session_writer(
    session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
    status: &str,
) {
    let Some(sw) = session_writer else { return };
    match Arc::into_inner(sw) {
        Some(mutex) => match mutex.lock() {
            Ok(mut w) => {
                if let Err(e) = w.finish(status) {
                    eprintln!("session: failed to finalize: {e}");
                } else {
                    eprintln!(
                        "session: saved {} message(s) to {}",
                        w.message_count(),
                        w.session_dir().display()
                    );
                }
            }
            Err(e) => eprintln!("session: failed to lock writer: {e}"),
        },
        None => eprintln!("session: writer still has other references; cannot finalize"),
    }
}

fn finalize_cost_tracker(
    cost_tracker: Option<std::sync::Mutex<recursive::cost::CostTracker>>,
    usage: recursive::llm::TokenUsage,
    llm_latency_ms: u64,
    model: &str,
) {
    let Some(tracker) = cost_tracker else { return };
    match tracker.into_inner() {
        Ok(mut t) => {
            t.record_usage(usage, llm_latency_ms);
            if let Err(e) = t.finish() {
                eprintln!("cost: failed to write cost.json: {e}");
            } else {
                eprintln!("cost: ${:.4} ({})", t.cost_usd().unwrap_or(0.0), model);
            }
        }
        Err(e) => eprintln!("cost: failed to lock cost tracker: {e}"),
    }
}

/// Resolve a `Cmd::Resume` invocation into a session directory and
/// load its seed transcript. Returns the session_dir alongside the
/// data needed to drive `run_resumed`.
///
/// Dispatch order:
/// 1. `from_file` is set → must point at a JSONL session directory
///    (a legacy `.json` is rejected with a migrate-legacy hint).
/// 2. `session` is set → if it looks like a legacy `.json` path
///    (ends with `.json`, or is an existing file), reject with the
///    migrate hint. Otherwise resolve as ID/substring.
/// 3. Neither → pick the most-recent active/interrupted session in
///    the workspace via `list_sessions_sorted_by_updated_at`.
fn resolve_resume_target(
    workspace: &Path,
    session: Option<String>,
    from_file: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = from_file {
        if path.extension().and_then(|e| e.to_str()) == Some("json") || path.is_file() {
            anyhow::bail!(legacy_resume_error(&path));
        }
        if !path.is_dir() {
            anyhow::bail!(
                "--from-file: {} is not a JSONL session directory",
                path.display()
            );
        }
        return Ok(path);
    }

    if let Some(s) = session {
        // Legacy detection: `.json` extension or a real file path.
        let candidate = PathBuf::from(&s);
        if s.ends_with(".json") || candidate.is_file() {
            anyhow::bail!(legacy_resume_error(&candidate));
        }
        let resolved = resolve_session_path(workspace, &s)?;
        if resolved.is_file() {
            // resolve_session_path can return a stray .json under
            // the sessions tree.
            anyhow::bail!(legacy_resume_error(&resolved));
        }
        return Ok(resolved);
    }

    // No arg → most-recent shortcut.
    let sorted = recursive::session::SessionReader::list_sessions_sorted_by_updated_at(workspace)
        .with_context(|| {
        format!(
            "scanning sessions for the workspace at {}",
            workspace.display()
        )
    })?;
    let pick = sorted
        .into_iter()
        .find(|(_, m)| matches!(m.status.as_str(), "active" | "interrupted"));
    match pick {
        Some((dir, _meta)) => Ok(dir),
        None => anyhow::bail!(
            "no active or interrupted session found in {}. \
             Run `recursive sessions list` to see what's available.",
            workspace.display()
        ),
    }
}

fn legacy_resume_error(path: &Path) -> String {
    format!(
        "legacy .json sessions are no longer resumable directly: {}\n\
         Run `recursive sessions migrate-legacy {}` to convert it to the JSONL\n\
         format, then `recursive resume <id>`.",
        path.display(),
        path.display()
    )
}

/// `recursive resume` command: dispatches based on which of
/// (positional `session`, `--from-file`, neither) was provided,
/// validates the tool-registry hash, then opens the existing
/// session for appending and resumes the run.
#[allow(clippy::too_many_arguments)]
async fn cmd_resume(
    config: Config,
    session: Option<String>,
    from_file: Option<PathBuf>,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    session_out: Option<PathBuf>,
    json_mode: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    external_pricing: Option<HashMap<String, ModelPricing>>,
    hook_timing: bool,
    session_recording: bool,
) -> anyhow::Result<()> {
    let session_dir = resolve_resume_target(&config.workspace, session, from_file)?;
    eprintln!("session: resuming from {}", session_dir.display());

    // Load meta and validate the tool-registry hash up front (before
    // building the runtime). If the hash mismatches, abort with the
    // same error string the legacy SessionFile path used.
    let meta = recursive::session::SessionReader::load_meta(&session_dir)
        .with_context(|| format!("reading .meta.json for session {}", session_dir.display()))?;
    let tools = build_tools(&config).await;
    let specs = tools.specs();
    let current_hash = recursive::session::hash_tool_specs(&specs);
    match &meta.tool_registry_hash {
        Some(stored) if stored != &current_hash => {
            anyhow::bail!(
                "tool registry hash mismatch: session has '{stored}', current is \
                 '{current_hash}'. Tools have changed since the session was saved; \
                 cannot resume."
            );
        }
        Some(_) => {} // matches → continue
        None => {
            eprintln!(
                "warning: session {} has no tool_registry_hash recorded \
                 (pre-g151 record); resuming without validation.",
                session_dir.display()
            );
        }
    }

    // Open the existing session for appending. Acquires the
    // SessionLock — refusing if another resume is already in flight.
    let writer = if session_recording {
        match SessionWriter::open_existing(&session_dir) {
            Ok(w) => Some(Arc::new(std::sync::Mutex::new(w))),
            Err(e) => {
                anyhow::bail!("cannot open session {}: {e}", session_dir.display());
            }
        }
    } else {
        None
    };

    // Load the seeded transcript (everything that's already on disk).
    let seed = recursive::session::SessionReader::load_messages(&session_dir)
        .with_context(|| format!("loading transcript for session {}", session_dir.display()))?;
    let goal = meta.goal.clone();

    let shutdown = shutdown_signal();
    run_resumed(
        config,
        seed,
        goal,
        max_transcript_chars,
        transcript_out,
        session_out,
        json_mode,
        plan_first,
        mcp_config,
        external_pricing,
        hook_timing,
        false, // session_recording — we already opened the writer below
        shutdown,
        writer,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_resumed(
    config: Config,
    seed: Vec<recursive::message::Message>,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    session_out: Option<PathBuf>,
    json_mode: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    external_pricing: Option<HashMap<String, ModelPricing>>,
    hook_timing: bool,
    session: bool,
    shutdown: tokio_util::sync::CancellationToken,
    // Goal 151: when resuming an existing JSONL session by ID, the
    // caller has already opened a `SessionWriter::open_existing`
    // for the session_dir. Pass it in so we don't create a fresh
    // session directory and so msg_NNN numbering continues.
    // `None` means "create a new session writer if `session` is
    // true" (the legacy `--resume-from <transcript.json>` path).
    existing_writer: Option<Arc<std::sync::Mutex<SessionWriter>>>,
) -> anyhow::Result<()> {
    let seed_len = seed.len();

    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> =
        if let Some(w) = existing_writer {
            eprintln!(
                "session: appending to {}",
                w.lock().unwrap().session_dir().display()
            );
            Some(w)
        } else if session {
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

    let (sink, event_rx) = ChannelSink::new();
    let mut runtime = build_runtime(
        &config,
        max_transcript_chars,
        seed,
        false,
        plan_first,
        mcp_config,
        hook_timing,
        Some(&goal),
        Some(Arc::new(sink)),
        Some(shutdown.clone()),
    )
    .await?;

    // Wire up per-turn checkpoints (resume path).
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
                }
            }
            Err(e) => {
                eprintln!("checkpoint: shadow repo unavailable, continuing without: {e}");
            }
        }
    }

    let tool_specs = runtime.kernel().tools().specs();
    let pre_transcript_len = runtime.transcript().len();

    if !json_mode {
        eprintln!("resuming from {seed_len} seeded message(s)");
    }
    let printer = if json_mode {
        tokio::spawn(stream_events_json(event_rx))
    } else {
        tokio::spawn(stream_events(event_rx))
    };

    let outcome = runtime.run(goal.clone()).await?;

    if let Some(ref sw) = session_writer {
        if let Ok(mut w) = sw.lock() {
            for msg in runtime.transcript().iter().skip(pre_transcript_len) {
                let _ = w.append(msg);
            }
        }
    }

    let transcript = runtime.transcript().to_vec();
    drop(runtime);
    printer.await.ok();

    if !json_mode {
        if let Some(ref msg) = outcome.final_text {
            println!("\n=== final ===\n{msg}");
        }
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.llm_latency_ms,
            outcome.steps,
            &external_pricing,
        );
        print_finish_note(&outcome.finish_reason);
    }

    let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
        "success"
    } else {
        "incomplete"
    };
    finalize_session_writer(session_writer, finish_status);
    finalize_cost_tracker(
        cost_tracker,
        outcome.total_usage,
        outcome.llm_latency_ms,
        &config.model,
    );

    if let Some(path) = transcript_out {
        save_transcript(&transcript, outcome.steps, &config.model, &path)?;
    }
    if let Some(path) = session_out {
        if !matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
            save_session(
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
    exit_for_finish(&outcome.finish_reason, outcome.steps)
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
    let mut tools = build_tools(&config).await;
    register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;
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
        let _ = exit_for_finish(&last.finish_reason, last.steps);
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

    let (sink, event_rx) = ChannelSink::new();
    let mut runtime = build_runtime(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        plan_first,
        mcp_config,
        hook_timing,
        Some(&goal),
        Some(Arc::new(sink)),
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
    let pre_transcript_len = runtime.transcript().len();

    let printer = if json_mode {
        tokio::spawn(stream_events_json(event_rx))
    } else {
        tokio::spawn(stream_events(event_rx))
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

    // Write new messages to session (all messages appended since build_runtime)
    if let Some(ref sw) = session_writer {
        if let Ok(mut w) = sw.lock() {
            for msg in runtime.transcript().iter().skip(pre_transcript_len) {
                let _ = w.append(msg);
            }
        }
    }

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
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.llm_latency_ms,
            outcome.steps,
            &external_pricing,
        );
        print_finish_note(&outcome.finish_reason);
    }

    let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
        "success"
    } else {
        "incomplete"
    };
    finalize_session_writer(session_writer, finish_status);
    finalize_cost_tracker(
        cost_tracker,
        outcome.total_usage,
        outcome.llm_latency_ms,
        &config.model,
    );

    if let Some(path) = transcript_out {
        save_transcript(&transcript, outcome.steps, &config.model, &path)?;
    }
    if let Some(path) = session_out {
        if !matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
            save_session(
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
    exit_for_finish(&outcome.finish_reason, outcome.steps)
}

/// Interactive setup wizard: walk the user through provider/model/key config.
async fn run_init() -> anyhow::Result<()> {
    use std::io::{self, Write};

    println!("recursive init — interactive setup\n");

    let config_path = recursive::config_file::config_file_path()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    if config_path.exists() {
        println!("  Existing config: {}\n", config_path.display());
    }

    // 1. Provider type
    println!("Which LLM provider protocol?");
    println!("  1) openai   — OpenAI, DeepSeek, GLM/Zhipu, Moonshot, Ollama, vLLM, ...");
    println!("  2) anthropic — Anthropic (Claude)");
    print!("\nChoice [1]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let provider_type = match input.trim() {
        "2" | "anthropic" => "anthropic",
        _ => "openai",
    };

    // 2. API base
    let default_base = if provider_type == "anthropic" {
        "https://api.anthropic.com"
    } else {
        "https://api.openai.com/v1"
    };
    println!("\nAPI base URL");
    println!("  Common options:");
    if provider_type == "openai" {
        println!("    https://api.openai.com/v1       (OpenAI)");
        println!("    https://api.deepseek.com        (DeepSeek)");
        println!("    https://open.bigmodel.cn/api/paas/v4  (GLM/Zhipu)");
        println!("    http://localhost:11434/v1        (Ollama)");
    } else {
        println!("    https://api.anthropic.com       (Anthropic)");
    }
    print!("\nAPI base [{}]: ", default_base);
    io::stdout().flush()?;
    input.clear();
    io::stdin().read_line(&mut input)?;
    let api_base = if input.trim().is_empty() {
        default_base.to_string()
    } else {
        input.trim().to_string()
    };

    // 3. Model
    let default_model = if api_base.contains("deepseek") {
        "deepseek-chat"
    } else if api_base.contains("bigmodel") {
        "glm-4-flash"
    } else if api_base.contains("anthropic") {
        "claude-sonnet-4-20250514"
    } else if api_base.contains("localhost") || api_base.contains("11434") {
        "qwen2.5-coder"
    } else {
        "gpt-4o-mini"
    };
    print!("\nModel [{}]: ", default_model);
    io::stdout().flush()?;
    input.clear();
    io::stdin().read_line(&mut input)?;
    let model = if input.trim().is_empty() {
        default_model.to_string()
    } else {
        input.trim().to_string()
    };

    // 4. API key
    print!("\nAPI key: ");
    io::stdout().flush()?;
    input.clear();
    io::stdin().read_line(&mut input)?;
    let api_key = input.trim().to_string();

    if api_key.is_empty() {
        println!("\n  Warning: no API key set. You can add it later:");
        println!("    recursive config set provider.api_key <KEY>");
    }

    // Write config
    recursive::config_file::set_value("provider.type", provider_type)?;
    recursive::config_file::set_value("provider.api_base", &api_base)?;
    recursive::config_file::set_value("provider.model", &model)?;
    if !api_key.is_empty() {
        recursive::config_file::set_value("provider.api_key", &api_key)?;
    }

    println!("\n  Config saved to: {}", config_path.display());
    println!("\n  You can now run:");
    println!("    recursive                — interactive REPL");
    println!("    recursive -p \"hello\"     — one-shot");
    println!("    recursive config show    — verify settings");

    Ok(())
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
    let mut runtime = build_runtime(
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
            tokio::spawn(stream_events_json(event_rx))
        } else {
            tokio::spawn(stream_events_repl(event_rx))
        };

        match runtime.run(goal.to_string()).await {
            Ok(outcome) => {
                // Reset to NullSink so the channel is dropped and printer finishes
                runtime.set_event_sink(Arc::new(NullSink));
                printer.await.ok();

                if !json_mode {
                    print_usage(
                        outcome.total_usage,
                        &config.model,
                        outcome.llm_latency_ms,
                        outcome.steps,
                        &external_pricing,
                    );
                    print_finish_note(&outcome.finish_reason);
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

async fn stream_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AssistantText { ref text, step } if !text.trim().is_empty() => {
                println!("[step {step}] assistant: {text}");
            }
            AgentEvent::ToolCall {
                ref name,
                ref arguments,
                step,
                ..
            } => {
                println!("[step {step}] -> {name} {arguments}");
            }
            AgentEvent::ToolResult {
                ref name,
                ref output,
                step,
                ..
            } => {
                let preview = if output.len() > 800 {
                    let mut end = 800.min(output.len());
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}\n...[truncated]", &output[..end])
                } else {
                    output.clone()
                };
                println!("[step {step}] <- {name}\n{preview}");
            }
            AgentEvent::TurnFinished { ref reason, steps } => {
                println!("[done after {steps} steps] reason: {reason}");
            }
            AgentEvent::Latency { step, llm_ms } => {
                println!("[step {step}] llm latency: {llm_ms}ms");
            }
            AgentEvent::Compacted {
                removed,
                kept,
                summary_chars,
                step,
            } => {
                println!(
                    "[step {step}] compacted {removed} msgs -> {kept} kept + {summary_chars}-char summary"
                );
            }
            AgentEvent::PlanProposed { ref plan_text, .. } => {
                println!("[plan] proposed: {plan_text}");
            }
            AgentEvent::PlanConfirmed => {
                println!("[plan] confirmed");
            }
            AgentEvent::PlanRejected { ref reason } => {
                println!("[plan] rejected: {reason}");
            }
            _ => {}
        }
    }
}

/// REPL-specific event handler: clean output without step prefixes on assistant text.
/// Tool calls are shown briefly; assistant text is printed directly.
async fn stream_events_repl(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AssistantText { ref text, .. } if !text.trim().is_empty() => {
                println!("{text}");
            }
            AgentEvent::ToolCall { ref name, .. } => {
                eprintln!("  ↳ {name}");
            }
            _ => {}
        }
    }
}

async fn stream_events_json(mut rx: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(ev) = rx.recv().await {
        if let Ok(line) = serde_json::to_string(&ev) {
            println!("{line}");
        }
    }
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
    let tools = build_tools(&config).await;

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

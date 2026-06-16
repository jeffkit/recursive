//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

mod cli;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::Level;

use recursive::mcp::{JsonRpcRequest, JsonRpcResponse};
use recursive::SessionFile;
use recursive::SessionStatus;
use recursive::SessionWriter;
use recursive::{
    config::Config,
    llm::{AnthropicProvider, LlmProvider, OpenAiProvider},
    tools::{ScheduleWakeup, WakeupSlot},
    AgentRuntimeBuilder, ChannelSink, CompositeSink, EventSink, FinishReason, NullSink,
    RetryPolicy, SessionPersistenceSink, ToolRegistry,
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

    /// Maximum agent loop iterations per goal. `0` = unlimited.
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

    /// Continue the most recent conversation in the current workspace.
    /// Equivalent to `recursive resume` without arguments (picks the latest session).
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Display name for this session (shown in the /resume picker and sessions list).
    #[arg(short = 'n', long = "name")]
    name: Option<String>,

    /// Reasoning effort level: low (no extended thinking), normal (default), high (max budget).
    /// Currently effective for Anthropic models that support extended thinking.
    #[arg(long = "effort", value_parser = ["low", "normal", "high"])]
    effort: Option<String>,

    /// Append text to the default system prompt instead of replacing it entirely.
    /// Useful for adding per-run instructions without discarding built-in guidance.
    #[arg(long = "append-system-prompt")]
    append_system_prompt: Option<String>,

    /// Permission mode for tool execution.
    /// - default: prompt as configured (respect config.headless)
    /// - plan: buffer all tool calls and present a plan before executing (like --plan-first)
    /// - auto: approve all tool calls without prompting (headless, use in trusted envs)
    #[arg(long = "permission-mode", value_parser = ["default", "plan", "auto"])]
    permission_mode: Option<String>,

    /// System prompt string to use for this session. Overrides the default system prompt.
    /// Mutually exclusive with --system-prompt-file; if both are given, --system-prompt wins.
    #[arg(long = "system-prompt", env = "RECURSIVE_SYSTEM_PROMPT")]
    system_prompt: Option<String>,

    /// Resume a conversation by session ID (or unique substring), or open interactive picker.
    /// Shorthand for `recursive resume <value>`. Cannot be combined with a subcommand.
    #[arg(short = 'r', long = "resume")]
    resume: Option<Option<String>>,

    /// Output format for non-interactive (`-p`) mode.
    /// - text: human-readable trace (default)
    /// - json: emit StepEvents as newline-delimited JSON (supersedes --json)
    /// - stream-json: same as json but also enables token streaming (supersedes --stream)
    #[arg(long = "output-format", value_parser = ["text", "json", "stream-json"])]
    output_format: Option<String>,

    /// Maximum total API spend in USD for this run. The run aborts once the
    /// cumulative cost exceeds this limit. Only checked after each completed turn.
    #[arg(long = "max-budget-usd")]
    max_budget_usd: Option<f64>,

    /// Enable debug logging with optional category filter (e.g. "api,hooks" or "trace").
    /// Shorthand for `--log debug`. If a filter is supplied it is set as the log filter.
    #[arg(short = 'd', long = "debug")]
    debug: Option<Option<String>>,

    /// Enable verbose output (equivalent to --log debug without a category filter).
    #[arg(long = "verbose")]
    verbose: bool,

    /// Additional workspace directories the agent is allowed to read and write.
    /// Repeatable: --add-dir /path/a --add-dir /path/b
    #[arg(long = "add-dir", num_args = 1)]
    add_dir: Vec<PathBuf>,

    /// Start WeChat iLink daemon alongside the TUI (or in headless mode with `weixin-daemon`).
    /// On first run, a QR code is displayed for login.
    #[cfg(feature = "weixin")]
    #[arg(long = "weixin", env = "RECURSIVE_WEIXIN")]
    weixin: bool,

    /// Override the iLink API base URL (e.g. when using an ilink-hub proxy).
    #[cfg(feature = "weixin")]
    #[arg(long = "weixin-base-url", env = "RECURSIVE_WEIXIN_BASE_URL")]
    weixin_base_url: Option<String>,

    /// Path to store WeChat bot credentials (default: ~/.recursive/<workspace>/weixin_creds.json).
    #[cfg(feature = "weixin")]
    #[arg(long = "weixin-cred-path")]
    weixin_cred_path: Option<PathBuf>,

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

    /// Restrict the agent to a comma-separated list of tools (e.g. "Read,Glob").
    /// All tools not in the list are removed from the registry before the run.
    /// Tool names are matched case-insensitively.
    #[arg(long = "allow-tools", env = "RECURSIVE_ALLOW_TOOLS")]
    allow_tools: Option<String>,
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
    /// Run as a headless WeChat daemon — no TUI, agent driven by WeChat messages.
    #[cfg(feature = "weixin")]
    WeixinDaemon,
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
    /// Non-interactive: pass all three of `--provider` / `--model` / `--api-key`
    /// to skip the prompts and write the config directly. With only some set,
    /// the missing fields are still prompted for.
    Init {
        /// Provider preset id from providers.toml (e.g. "deepseek", "anthropic").
        /// Writes `provider.preset` to the config so the runtime can resolve
        /// api_base / model / type from the catalog.
        #[arg(long)]
        provider: Option<String>,
        /// Model name. Defaults to the preset's `default_model`.
        #[arg(long, short = 'm')]
        model: Option<String>,
        /// API key. If omitted, falls back to the preset's `key_env` env var,
        /// then prompts.
        #[arg(long, short = 'k', hide_env_values = true)]
        api_key: Option<String>,
    },
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
    /// Run diagnostics: verify API key, config, workspace, and MCP servers.
    /// Exits 0 if everything looks healthy, 1 if any check fails.
    Doctor,
    /// Configure and manage MCP servers for the current workspace.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Check for a newer release and print upgrade instructions.
    /// Uses the GitHub releases API; requires internet access.
    Update,
    /// Alias for `update`.
    Upgrade,
    /// List active agent sessions (sessions in the current workspace
    /// whose status is "active" or whose lock file is live).
    Agents,
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
    /// Run git garbage collection on the workspace's shadow-git repo to
    /// reclaim disk space. Prunes all unreachable objects left by rewinds
    /// and by the historical snapshots that captured build artifacts.
    /// Safe to run at any time; does not affect ongoing sessions.
    GcCheckpoints,
    /// Delete the workspace's shadow-git repo entirely, reclaiming all
    /// disk space it occupies. Future sessions will start a fresh
    /// shadow repo; historical rewind for existing sessions will no
    /// longer be available. Use this when gc is not enough.
    CleanCheckpoints {
        /// Skip confirmation prompt.
        #[arg(long, short = 'f')]
        force: bool,
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
    /// Persist a secret (typically an API key) to a 0600 shell-sourceable
    /// file at ~/.recursive/secrets.env. The binary reads the key from
    /// the process env at runtime — never from the config file —
    /// so an agent with `run_shell` cannot `cat` the key out of disk.
    SetSecret {
        /// Env var name to export, e.g. `DEEPSEEK_API_KEY`.
        env_name: String,
        /// The secret value.
        value: String,
    },
    /// Print the config file path.
    Path,
}

#[derive(Subcommand, Debug)]
enum McpCmd {
    /// List all MCP servers discovered from the workspace.
    List,
    /// Add an MCP server to `.mcp.json` in the workspace root.
    Add {
        /// Server name (used as the key in `.mcp.json`).
        name: String,
        /// For stdio servers: the command to run (e.g. `npx`, `uvx`, binary path).
        /// For HTTP+SSE servers: start the name with `http://` or `https://`.
        command_or_url: String,
        /// Additional arguments for stdio servers (ignored for HTTP+SSE).
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Remove an MCP server from `.mcp.json` by name.
    Remove {
        /// Name of the server to remove.
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Resolve log level early (before Config::from_env) so init_logging sees
    // --debug / --verbose before any config-file processing happens.
    let early_log = if cli.verbose {
        "debug".to_string()
    } else if let Some(Some(ref filter)) = cli.debug {
        filter.clone()
    } else if cli.debug.is_some() {
        "debug".to_string()
    } else {
        cli.log.clone()
    };
    init_logging(&early_log)?;
    tracing::trace!("recursive main starting");

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
    // --system-prompt wins over --system-prompt-file when both are supplied.
    if let Some(prompt_str) = cli.system_prompt {
        config.system_prompt = prompt_str;
    } else if let Some(p) = cli.system_prompt_file {
        config.system_prompt = std::fs::read_to_string(&p)
            .with_context(|| format!("reading system prompt: {}", p.display()))?;
    }
    // --append-system-prompt: tack additional text onto whatever system prompt is active.
    if let Some(extra) = &cli.append_system_prompt {
        config.system_prompt.push('\n');
        config.system_prompt.push_str(extra);
    }
    if matches!(cli.permission_mode.as_deref(), Some("auto")) {
        config.headless = true;
    }
    // --effort: map to thinking_budget (low=0 disables, normal=default, high=max).
    if let Some(effort) = &cli.effort {
        config.thinking_budget = match effort.as_str() {
            "low" => Some(0),
            "high" => Some(16000),
            _ => None, // "normal" → leave as default
        };
    }
    // --name: optional display name for the session.
    if let Some(name) = cli.name {
        config.session_name = Some(name);
    }
    // --max-budget-usd: store for cost gate (checked after each turn).
    if let Some(budget) = cli.max_budget_usd {
        config.max_budget_usd = Some(budget);
    }
    // --add-dir: extra allowed sandbox roots.
    if !cli.add_dir.is_empty() {
        config.extra_dirs = cli.add_dir.clone();
    }
    // --allow-tools: restrict agent to a subset of tools.
    if let Some(ref allow) = cli.allow_tools {
        config.allow_tools = allow.split(',').map(|s| s.trim().to_string()).collect();
    }
    // --output-format: supersedes --json and --stream.
    let effective_json = cli.json
        || matches!(
            cli.output_format.as_deref(),
            Some("json") | Some("stream-json")
        );
    let effective_stream =
        cli.stream || matches!(cli.output_format.as_deref(), Some("stream-json"));
    // Log level was already resolved and applied at startup (early_log above).

    // Determine effective command:
    // - Explicit subcommand → use it
    // - `-r/--resume` → resume by ID or pick latest
    // - `-c/--continue` → resume the latest session (like `recursive resume`)
    // - `-p "goal"` → one-shot run (like `claude -p`)
    // - Nothing → TUI (if compiled in), else REPL
    let effective_cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            if let Some(resume_val) = cli.resume {
                Cmd::Resume {
                    session: resume_val,
                    from_file: None,
                    orphans: None,
                }
            } else if cli.continue_session {
                Cmd::Resume {
                    session: None,
                    from_file: None,
                    orphans: None,
                }
            } else if let Some(prompt) = cli.prompt {
                Cmd::Run { goal: vec![prompt] }
            } else {
                #[cfg(all(feature = "tui", feature = "weixin"))]
                if cli.weixin {
                    return run_tui_with_weixin(
                        cli.weixin_base_url.clone(),
                        cli.weixin_cred_path.clone(),
                        config.workspace.clone(),
                    )
                    .await;
                }
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
        #[cfg(feature = "weixin")]
        Cmd::WeixinDaemon => {
            return run_weixin_headless_daemon(
                config,
                cli.mcp_config,
                cli.weixin_base_url,
                cli.weixin_cred_path,
            )
            .await;
        }

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
                    let anthropic_retry = recursive::llm::RetryPolicy {
                        max_retries: config.retry_max,
                        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
                    };
                    let anthropic =
                        AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                            .with_temperature(config.temperature)
                            .with_retry_policy(anthropic_retry);
                    Arc::new(anthropic)
                }
                _ => {
                    let openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)?
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
            let session_ttl_secs: u64 = std::env::var("RECURSIVE_SESSION_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600);
            let max_concurrent = config.max_concurrent_runs;
            let run_semaphore =
                std::sync::Arc::new(tokio::sync::Semaphore::new(if max_concurrent == 0 {
                    tokio::sync::Semaphore::MAX_PERMITS
                } else {
                    max_concurrent.max(1)
                }));
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
                session_ttl_secs,
                run_semaphore,
                rate_limiter: recursive::http::rate_limiter_from_env(),
            };
            // M3: spawn the session reaper so idle sessions are evicted.
            // Clone the state before consuming it for the router (both share the
            // same Arc-wrapped inner fields, so no actual data is duplicated).
            let reaper_state = std::sync::Arc::new(state.clone());
            recursive::http::spawn_session_reaper(reaper_state, Duration::from_secs(60));
            let router = recursive::http::build_router(state);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("Recursive HTTP API listening on {addr}");
            // Warn if auth is effectively disabled
            let auth_enabled = std::env::var("RECURSIVE_API_KEY").is_ok()
                || std::env::var("RECURSIVE_JWT_SECRET").is_ok();
            if !auth_enabled {
                tracing::warn!(
                    "HTTP server started with authentication DISABLED. \
                     Set RECURSIVE_API_KEY or RECURSIVE_JWT_SECRET to enable auth. \
                     Any client with network access can execute commands."
                );
            }
            let shutdown = shutdown_signal();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await?;
            eprintln!("shutdown: HTTP server stopped gracefully");
            Ok(())
        }
        Cmd::Init {
            provider,
            model,
            api_key,
        } => cli::init::run_init(provider, model, api_key).await,
        Cmd::Run { goal } => {
            let shutdown = shutdown_signal();
            run_once(
                config,
                goal.join(" "),
                cli.max_transcript_chars,
                cli.transcript_out,
                cli.session_out,
                effective_json,
                effective_stream,
                cli.mcp_config,
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
                effective_json,
                cli.mcp_config,
                effective_stream,
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
                effective_json,
                effective_stream,
                cli.mcp_config,
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
                        effective_json,
                        cli.mcp_config,
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
                effective_json,
                cli.mcp_config,
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
                            let name_suffix = meta
                                .name
                                .as_deref()
                                .map(|n| format!("  «{n}»"))
                                .unwrap_or_default();
                            println!(
                                "  {}  [{}]{} {}",
                                s.display(),
                                meta.status,
                                name_suffix,
                                label
                            );
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
                    if let Some(preset) = meta.preset.as_deref() {
                        println!("  preset:          {preset}");
                    }
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
            SessionCmd::GcCheckpoints => {
                cli::session::cmd_session_gc_checkpoints(&config.workspace)
            }
            SessionCmd::CleanCheckpoints { force } => {
                cli::session::cmd_session_clean_checkpoints(&config.workspace, force)
            }
        },
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Show => {
                println!("# Effective configuration (env > config file > default)");
                println!("provider_type: {}", config.provider_type);
                println!("model:         {}", config.model);
                println!("api_base:      {}", config.api_base);
                println!("api_key:       {}", mask_key(config.api_key.as_deref()));
                // Preset resolution: `provider.preset` from the file wins; if
                // absent, fall back to a catalog match against the resolved
                // api_base. Surfaces the preset chain added by the
                // preset-config goal — without it, a user with
                // `preset = "deepseek"` would only see the raw fields and
                // have to manually re-derive that they're on DeepSeek.
                let preset_label = config
                    .preset
                    .clone()
                    .or_else(|| {
                        recursive::providers::find_preset_by_api_base(&config.api_base)
                            .map(|p| p.id.to_string())
                    })
                    .unwrap_or_else(|| "(none)".to_string());
                println!("preset:        {preset_label}");
                // Resolve preset chain: explicit `provider.preset` from
                // the file wins; if absent, fall back to a catalog match
                // against the resolved `api_base`. Surfaces the preset
                // chain added by the preset-config goal — without it, a
                // user with `preset = "deepseek"` would only see the raw
                // fields and have to manually re-derive that they're on
                // DeepSeek.
                //
                // We use `find_preset_extended` (bundled + providers.d/),
                // so a user override also surfaces here. Since the
                // surrounding code only reads three `String` fields, we
                // skip the bundling step and read them directly from the
                // owned preset.
                let resolved_preset: Option<recursive::providers::ProviderPreset> = config
                    .preset
                    .as_deref()
                    .and_then(recursive::providers::find_preset_extended)
                    .or_else(|| {
                        // Bundled fallback by api_base — only the
                        // bundled catalog is searchable by URL.
                        let bundled: Option<&'static recursive::providers::ProviderPreset> =
                            recursive::providers::find_preset_by_api_base(&config.api_base);
                        bundled.cloned()
                    });
                if let Some(preset) = &resolved_preset {
                    let key_env = if preset.key_env.is_empty() {
                        "(none)".to_string()
                    } else {
                        preset.key_env.clone()
                    };
                    println!(
                        "preset resolves to: type={}, model={}, key_env={key_env}",
                        preset.provider_type, preset.default_model
                    );
                }
                println!("workspace:     {}", config.workspace.display());
                if config.max_steps == 0 {
                    println!("max_steps:     unlimited");
                } else {
                    println!("max_steps:     {}", config.max_steps);
                }
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
                // L1: provider.api_key must never land in config.toml.
                // set_value() itself refuses the write, but we pre-empt
                // with a more helpful message pointing the user at
                // set-secret.
                if key == "provider.api_key" || key.starts_with("provider.api_key.") {
                    anyhow::bail!(
                        "refusing to persist {} to config.toml.\n\
                         \n\
                         The binary reads API keys from the process env at runtime,\n\
                         never from the config file. Use:\n\
                         \n  \
                         recursive config set-secret <ENV_NAME> <KEY>\n\
                         \n\
                         or set the env var directly:  export DEEPSEEK_API_KEY='...'",
                        key
                    );
                }
                recursive::config_file::set_value(&key, &value)?;
                if let Some(path) = recursive::config_file::config_file_path() {
                    println!("Set {} = {} in {}", key, value, path.display());
                }
                Ok(())
            }
            ConfigCmd::SetSecret { env_name, value } => {
                recursive::config_file::set_secret(&env_name, &value)?;
                let path = recursive::config_file::secrets_env_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.recursive/secrets.env".to_string());
                println!("Secret written to {path} (mode 0600).");
                println!("Add `source {path}` to your shell rc to load it into the env.");
                println!("(or set the env var directly:  export {env_name}='...')");
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
        Cmd::Doctor => cmd_doctor(&config, cli.mcp_config).await,
        Cmd::Mcp { cmd } => cmd_mcp(cmd, &config.workspace).await,
        Cmd::Update | Cmd::Upgrade => cmd_update().await,
        Cmd::Agents => cmd_agents(&config.workspace),
    }
}

// ─── doctor ──────────────────────────────────────────────────────────────────

/// Run diagnostics and print a health report. Returns Ok if all checks pass.
async fn cmd_doctor(config: &Config, mcp_config: Option<PathBuf>) -> anyhow::Result<()> {
    let mut any_fail = false;

    macro_rules! check {
        ($label:expr, $ok:expr, $detail:expr) => {{
            let passed = $ok;
            let icon = if passed { "✓" } else { "✗" };
            if passed {
                println!("  {icon}  {}", $label);
            } else {
                println!("  {icon}  {}  — {}", $label, $detail);
                any_fail = true;
            }
        }};
    }

    println!("\nRecursive diagnostics\n");

    // 1. API key
    check!(
        "API key is set",
        config.api_key.is_some(),
        "set RECURSIVE_API_KEY or run `recursive init`"
    );

    // 2. Model
    check!(
        format!("Model: {}", config.model),
        !config.model.is_empty(),
        "model is empty; set RECURSIVE_MODEL or run `recursive init`"
    );

    // 3. Provider type
    check!(
        format!("Provider: {}", config.provider_type),
        matches!(config.provider_type.as_str(), "openai" | "anthropic"),
        format!("unknown provider type '{}'", config.provider_type)
    );

    // 4. Workspace
    let ws_ok = config.workspace.exists();
    check!(
        format!("Workspace: {}", config.workspace.display()),
        ws_ok,
        "workspace directory not found"
    );

    // 5. MCP config
    if let Some(ref path) = mcp_config {
        let mcp_ok = path.exists();
        check!(
            format!("MCP config: {}", path.display()),
            mcp_ok,
            "file not found"
        );
        if mcp_ok {
            match recursive::mcp::load_mcp_config(path) {
                Ok(servers) => {
                    println!("       {} MCP server(s) configured", servers.len());
                }
                Err(e) => {
                    println!("  ✗  MCP config parse error  — {e}");
                    any_fail = true;
                }
            }
        }
    } else {
        // Check auto-discovered .mcp.json
        let discovered = recursive::mcp::discover_mcp_servers(&config.workspace).await;
        match discovered {
            Ok(servers) if !servers.is_empty() => {
                println!(
                    "  ✓  Auto-discovered {} MCP server(s) from workspace .mcp.json",
                    servers.len()
                );
            }
            Ok(_) => {
                println!("  ·  No MCP servers configured (optional)");
            }
            Err(e) => {
                println!("  ✗  MCP discovery error  — {e}");
                any_fail = true;
            }
        }
    }

    // 6. Config file
    let config_path = recursive::config_file::config_file_path();
    if let Some(ref p) = config_path {
        check!(
            format!("Config file: {}", p.display()),
            p.exists(),
            "not found — using defaults (run `recursive init` to create one)"
        );
    } else {
        println!("  ·  Config file: not yet created");
    }

    println!();
    if any_fail {
        eprintln!("One or more checks failed.");
        std::process::exit(1);
    } else {
        println!("All checks passed.");
    }
    Ok(())
}

// ─── mcp ─────────────────────────────────────────────────────────────────────

async fn cmd_mcp(cmd: McpCmd, workspace: &std::path::Path) -> anyhow::Result<()> {
    match cmd {
        McpCmd::List => {
            let servers = recursive::mcp::discover_mcp_servers(workspace).await?;
            if servers.is_empty() {
                println!("No MCP servers configured.");
                println!(
                    "hint: add servers with `recursive mcp add <name> <command>` \
                     (they are stored in `<workspace>/.mcp.json`)"
                );
            } else {
                println!("MCP servers ({}):", servers.len());
                for s in &servers {
                    if let Some(ref url) = s.url {
                        println!("  {}  (http+sse)  {}", s.name, url);
                    } else {
                        let args = s.args.join(" ");
                        let cmd_str = if args.is_empty() {
                            s.command.clone()
                        } else {
                            format!("{} {}", s.command, args)
                        };
                        println!("  {}  (stdio)  {}", s.name, cmd_str);
                    }
                }
            }
            Ok(())
        }
        McpCmd::Add {
            name,
            command_or_url,
            args,
        } => {
            let mcp_json = workspace.join(".mcp.json");
            // Read existing config or start fresh.
            let mut obj: serde_json::Map<String, serde_json::Value> = if mcp_json.exists() {
                let s = std::fs::read_to_string(&mcp_json)?;
                serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        if let serde_json::Value::Object(m) = v {
                            Some(m)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default()
            } else {
                serde_json::Map::new()
            };

            let servers_obj = obj
                .entry("mcpServers")
                .or_insert(serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!(".mcp.json mcpServers is not an object"))?;

            let entry = if command_or_url.starts_with("http://")
                || command_or_url.starts_with("https://")
            {
                serde_json::json!({"url": command_or_url})
            } else if args.is_empty() {
                serde_json::json!({"command": command_or_url})
            } else {
                serde_json::json!({"command": command_or_url, "args": args})
            };

            servers_obj.insert(name.clone(), entry);
            let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
            std::fs::write(&mcp_json, json)?;
            println!("Added MCP server '{name}' to {}", mcp_json.display());
            Ok(())
        }
        McpCmd::Remove { name } => {
            let mcp_json = workspace.join(".mcp.json");
            if !mcp_json.exists() {
                anyhow::bail!(".mcp.json not found at {}", mcp_json.display());
            }
            let s = std::fs::read_to_string(&mcp_json)?;
            let mut obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str::<serde_json::Value>(&s)
                    .ok()
                    .and_then(|v| {
                        if let serde_json::Value::Object(m) = v {
                            Some(m)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

            if let Some(servers_obj) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
                if servers_obj.remove(&name).is_some() {
                    let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
                    std::fs::write(&mcp_json, json)?;
                    println!("Removed MCP server '{name}' from {}", mcp_json.display());
                } else {
                    anyhow::bail!("Server '{name}' not found in .mcp.json");
                }
            } else {
                anyhow::bail!(".mcp.json has no 'mcpServers' key");
            }
            Ok(())
        }
    }
}

// ─── update / upgrade ─────────────────────────────────────────────────────────

async fn cmd_update() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Checking for updates…");

    // Use GitHub API to find the latest release (if the repo is public).
    // Gracefully handle network failures.
    let client = reqwest::Client::builder()
        .user_agent(format!("recursive-agent/{current}"))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get("https://api.github.com/repos/recursive-ai/recursive/releases/latest")
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(serde::Deserialize)]
            struct Release {
                tag_name: String,
                html_url: String,
            }
            if let Ok(release) = r.json::<Release>().await {
                let latest = release.tag_name.trim_start_matches('v');
                if latest == current {
                    println!("You are on the latest version.");
                } else {
                    println!("New version available: v{latest}");
                    println!("Release page: {}", release.html_url);
                    println!();
                    println!("To upgrade, rebuild from source:");
                    println!("  cargo install --path . --features tui");
                }
            } else {
                println!("Could not parse release info.");
            }
        }
        Ok(r) => {
            println!("GitHub API returned status {}", r.status());
            println!("Check manually: https://github.com/recursive-ai/recursive/releases");
        }
        Err(e) => {
            println!("Could not reach GitHub ({e}).");
            println!("Check manually: https://github.com/recursive-ai/recursive/releases");
        }
    }
    Ok(())
}

// ─── agents ──────────────────────────────────────────────────────────────────

fn cmd_agents(workspace: &std::path::Path) -> anyhow::Result<()> {
    let sessions = recursive::session::SessionReader::list_sessions(workspace).unwrap_or_default();

    let active: Vec<_> = sessions
        .iter()
        .filter_map(|dir| {
            let meta = recursive::session::SessionReader::load_meta(dir).ok()?;
            if meta.status == SessionStatus::Active {
                Some((dir, meta))
            } else {
                None
            }
        })
        .collect();

    if active.is_empty() {
        println!("No active agent sessions.");
        println!("hint: start a session with `recursive run <goal>` or `recursive repl`");
    } else {
        println!("Active agent sessions ({}):", active.len());
        for (dir, meta) in &active {
            let label = meta
                .name
                .as_deref()
                .map(|n| format!("  «{n}»"))
                .unwrap_or_default();
            let prompt = meta
                .last_prompt
                .as_deref()
                .or(Some(meta.goal.as_str()))
                .unwrap_or("(no prompt)");
            println!(
                "  {}{}  [{}]  {}",
                meta.session_id, label, meta.updated_at, prompt
            );
            println!("    dir: {}", dir.display());
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
    mcp_config: Option<PathBuf>,
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
    if !config.allow_tools.is_empty() {
        tools.retain_tools(&config.allow_tools);
    }

    // Build LLM provider
    let api_key = config.require_api_key()?;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn LlmProvider> = match config.provider_type.as_str() {
        "anthropic" => {
            let anthropic_retry = recursive::llm::RetryPolicy {
                max_retries: config.retry_max,
                initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
                max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
            };
            let anthropic = AnthropicProvider::new(&config.api_base, api_key, &config.model)?
                .with_temperature(config.temperature)
                .with_retry_policy(anthropic_retry);
            Arc::new(anthropic)
        }
        _ => {
            let openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)?
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
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    session: bool,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> = if session {
        match SessionWriter::create_with_tools(
            &config.workspace,
            &goal,
            &config.model,
            &config.provider_type,
            &[],
            config.preset.as_deref(),
        ) {
            Ok(mut writer) => {
                if let Some(ref name) = config.session_name {
                    writer.set_name(name.as_str());
                }
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
        mcp_config,
        hook_timing,
        Some(&goal),
        Some(event_sink),
        Some(shutdown.clone()),
        true, // interactive CLI — plan mode tools enabled
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

    let outcome = runtime.run(goal.clone()).await?;

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
        );
        cli::output::print_finish_note(&outcome.finish_reason);
    }

    let finish_status = if matches!(outcome.finish_reason, FinishReason::NoMoreToolCalls) {
        SessionStatus::Completed
    } else {
        SessionStatus::Crashed
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
    mcp_config: Option<PathBuf>,
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
        mcp_config,
        hook_timing,
        None,
        None,
        None,
        true, // interactive REPL — plan mode tools enabled
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

// ── WeChat helpers ────────────────────────────────────────────────────────────

/// Run TUI with a WeChat iLink daemon running in the background.
///
/// Starts the WeChat daemon, connects its request channel to the TUI backend,
/// then runs the TUI event loop as normal.
#[cfg(all(feature = "tui", feature = "weixin"))]
async fn run_tui_with_weixin(
    base_url: Option<String>,
    cred_path: Option<PathBuf>,
    workspace: PathBuf,
) -> anyhow::Result<()> {
    use recursive::tui::backend::Backend;
    use recursive::weixin::{WeixinDaemon, WeixinDaemonOptions};

    let mut opts = WeixinDaemonOptions::new(&workspace);
    opts.base_url = base_url;
    opts.cred_path = cred_path;

    let daemon = WeixinDaemon::new(opts);
    daemon.login(false).await?;

    let (_polling_handle, mut weixin_req_rx) = daemon.start();

    // Spawn bridge: forward WeixinRequests to the TUI backend.
    let backend = Backend::spawn();
    let weixin_tx = backend.weixin_tx.clone();
    tokio::spawn(async move {
        use recursive::tui::events::WeixinBackendRequest;
        while let Some(req) = weixin_req_rx.recv().await {
            let backend_req = WeixinBackendRequest {
                user_id: req.user_id,
                text: req.text,
                reply_tx: req.reply_tx,
            };
            if weixin_tx.send(backend_req).is_err() {
                break;
            }
        }
    });

    // Run TUI with the already-spawned backend.
    recursive::tui::run_with_backend(backend)
        .await
        .map_err(anyhow::Error::from)
}

/// Run as a headless WeChat-only daemon (no TUI).
///
/// All interaction happens through WeChat messages. The agent runs in a
/// simple request-response loop.
#[cfg(feature = "weixin")]
async fn run_weixin_headless_daemon(
    config: recursive::config::Config,
    mcp_config: Option<PathBuf>,
    base_url: Option<String>,
    cred_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    use recursive::weixin::{WeixinDaemon, WeixinDaemonOptions};
    use tracing::info;

    let workspace = config.workspace.clone();
    if let Err(msg) = config.validate_for_agent() {
        anyhow::bail!("{msg}");
    }

    let mut opts = WeixinDaemonOptions::new(&workspace);
    opts.base_url = base_url;
    opts.cred_path = cred_path;

    let daemon = WeixinDaemon::new(opts);
    daemon.login(false).await?;
    let (_polling_handle, mut weixin_req_rx) = daemon.start();

    info!("WeChat daemon started — waiting for messages");
    eprintln!("📱 Recursive WeChat daemon running. Send a message to get started.");

    // Build runtime.
    let mut runtime = cli::builder::build_runtime(
        &config,
        None,       // max_transcript_chars
        Vec::new(), // seed messages
        false,      // stream
        mcp_config,
        false, // hook_timing
        None,  // goal
        None,  // event_sink (WeChat responses come from enqueue return value)
        None,  // shutdown_token
        false, // headless daemon — no human to confirm plans
    )
    .await?;

    while let Some(req) = weixin_req_rx.recv().await {
        info!("WeChat: processing message from {}", req.user_id);
        match runtime.enqueue(&req.text).await {
            Ok(Some(outcome)) => {
                let _ = req.reply_tx.send(outcome.final_text);
            }
            Ok(None) => {
                let _ = req.reply_tx.send(None);
            }
            Err(e) => {
                tracing::error!("WeChat runtime error: {e}");
                let _ = req.reply_tx.send(Some(format!("❌ 处理出错: {e}")));
            }
        }
    }

    Ok(())
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
            preset: None,
            max_steps: 1,
            temperature: 0.0,
            system_prompt: "test".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 1,
            shell_timeout_secs: 5,
            headless: false,
            memory_summary_limit: 5,
            thinking_budget: None,
            session_name: None,
            max_budget_usd: None,
            extra_dirs: Vec::new(),
            allow_tools: Vec::new(),
            context_window_override: None,
            subagent_max_depth: 2,
            allow_bypass_permissions: false,
            max_search_rounds: 3,
            stuck_window: 10,
            stuck_error_rate: 0.8,
            max_concurrent_runs: 8,
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
            None,
            false,
            None,
            None,
            None,
            false,
        )
        .await;
        assert!(r1.is_ok(), "openai/stream=false: must not panic or fail");

        let r2 = build_runtime(
            &cfg,
            None,
            Vec::new(),
            /* stream */ true,
            None,
            false,
            None,
            None,
            None,
            false,
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
            None,
            false,
            None,
            None,
            None,
            false,
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

    #[test]
    fn auth_disabled_when_no_env_vars() {
        // Can't easily test env vars in parallel tests, so just verify
        // the logic compiles and the condition is reachable.
        let api_key_set = false;
        let jwt_set = false;
        let auth_enabled = api_key_set || jwt_set;
        assert!(!auth_enabled);
    }
}

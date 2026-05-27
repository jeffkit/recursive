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
use recursive::OnMessageFn;
use recursive::{
    config::Config,
    llm::{
        load_pricing_from_yaml, pricing_for, AnthropicProvider, LlmProvider, ModelPricing,
        OpenAiProvider, TokenUsage,
    },
    tools::{
        ApplyPatch, BackgroundJobManager, CheckBackground, EstimateTokens, Forget, ListDir,
        LoadSkill, LocalTransport, ReadFile, Recall, Remember, RunBackground, RunShell,
        RunSkillScript, ScheduleWakeup, SearchFiles, SubAgent, ToolTransport, WakeupSlot, WebFetch,
        WriteFile,
        ScratchpadDelete, ScratchpadGet, ScratchpadList, WorkingMemoryTool,
    },
    tools::{
        ForgetFact, RecallFact, RememberFact, UpdateFact,
    },
    tools::{
        EpisodicRecall,
    },
    Agent, AgentRunner, FinishReason, PlanningMode, RetryPolicy, StepEvent, ToolRegistry,
    TranscriptFile,
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

    /// Enable live session recording via SessionWriter. Every message is
    /// written to a JSONL file under .recursive/sessions/<slug>/<session-id>/.
    /// The session directory path is printed to stderr on completion.
    #[arg(long, env = "RECURSIVE_SESSION")]
    session: bool,

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
    /// Resume a run from a saved session file.
    Resume {
        /// Path to the session JSON file (as written by --session-out).
        #[arg(required = true)]
        session: PathBuf,
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
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// List all session files in the workspace's session directory.
    List,
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
                config: config.clone(),
                provider,
                sessions: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                event_channels: std::sync::Arc::new(tokio::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
            };
            let router = recursive::http::build_router(state);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("Recursive HTTP API listening on {addr}");
            axum::serve(listener, router).await?;
            Ok(())
        }
        Cmd::Init => run_init().await,
        Cmd::Run { goal } => {
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
                cli.session,
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
                        cli.session,
                    )
                    .await
                }
            }
        }
        Cmd::Resume { session } => {
            let session_file = SessionFile::read_from(&session)
                .with_context(|| format!("reading session file: {}", session.display()))?;
            let tools = build_tools(&config).await;
            let specs = tools.specs();
            session_file
                .validate_tool_registry(&specs)
                .map_err(|msg| anyhow::anyhow!("{}", msg))?;
            let goal = session_file.goal.clone();
            let seed = session_file.into_transcript();
            run_resumed(
                config,
                seed,
                goal,
                cli.max_transcript_chars,
                cli.transcript_out,
                cli.session_out,
                cli.json,
                cli.plan_first,
                cli.mcp_config,
                external_pricing,
                cli.hook_timing,
                cli.session,
            )
            .await
        }
        Cmd::Sessions { cmd } => match cmd {
            SessionCmd::List => {
                let sessions = recursive::session::list_sessions(&config.workspace)?;
                if sessions.is_empty() {
                    println!(
                        "No sessions found in {}",
                        config
                            .workspace
                            .join(".recursive")
                            .join("sessions")
                            .display()
                    );
                } else {
                    println!("Session files ({}):", sessions.len());
                    for s in &sessions {
                        println!("  {}", s.display());
                    }
                }
                Ok(())
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
    }
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
    registry = registry
        .register(Arc::new(EpisodicRecall::new(root)));
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
    registry
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

/// Build an agent, optionally registering MCP tools from a config file.
#[allow(clippy::too_many_arguments)]
async fn build_agent(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    plan_first: bool,
    mcp_config: Option<PathBuf>,
    hook_timing: bool,
    goal: Option<&str>,
    on_message: Option<recursive::OnMessageFn>,
) -> anyhow::Result<(Agent, mpsc::UnboundedReceiver<StepEvent>)> {
    let api_key = config.require_api_key()?;
    // Provider selector: `openai` (default) uses the OpenAI-compatible adapter,
    // `anthropic` switches to the Anthropic Messages API adapter.
    let provider_type = &config.provider_type;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let mut openai = OpenAiProvider::new(&config.api_base, api_key, &config.model)
        .with_temperature(config.temperature)
        .with_retry_policy(retry);
    if stream {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        openai = openai.with_stream_tx(tx);
    }
    let provider: Arc<dyn LlmProvider> = match provider_type.as_str() {
        "anthropic" => {
            // AnthropicProvider has its own RetryPolicy type; mirror the same
            // values from the shared Config so behavior is consistent.
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
        _ => Arc::new(openai),
    };
    let mut tools = build_tools(config).await;
    register_mcp_tools(&mut tools, &config.workspace, mcp_config).await;

    // Conditionally register sub-agent tool (opt-in via env var)
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

    // Load project context from AGENTS.md if present
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
    // Inject auto-loaded skill bodies (Always + Trigger matching goal)
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
    // When sub-agent is enabled, append a hint about its usage
    let system_prompt = if sub_agent_enabled {
        format!(
            "{}\n\nWhen you need to do focused research or scan files without polluting your main context, use the `sub_agent` tool. It spawns a fresh agent with its own transcript and a restricted tool set (read-only by default).",
            system_prompt
        )
    } else {
        system_prompt
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let mut builder = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt(&system_prompt)
        .max_steps(config.max_steps)
        .events(tx);
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
    // Optional Compactor wiring. Default off; set
    // RECURSIVE_COMPACT_THRESHOLD=<chars> to enable. When the transcript
    // grows past `chars` characters, the agent asks the model to
    // summarize older messages, freeing context budget.
    if let Ok(threshold) = std::env::var("RECURSIVE_COMPACT_THRESHOLD") {
        if let Ok(n) = threshold.parse::<usize>() {
            if n > 0 {
                builder = builder.compactor(recursive::Compactor::new(n));
            }
        }
    }
    if hook_timing {
        builder = builder.hook(Arc::new(recursive::hooks::ToolTimingHook::new()));
    }
    builder = builder.streaming(stream);
    if plan_first {
        builder = builder.planning_mode(PlanningMode::PlanFirst);
    }
    if let Some(cb) = on_message {
        builder = builder.on_message(cb);
    }
    let agent = builder.build()?;
    Ok((agent, rx))
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
    if let FinishReason::TranscriptLimit { chars, limit } = finish {
        eprintln!(
            "note: stopped because transcript reached {} chars (limit {})",
            chars, limit
        );
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
    outcome: &recursive::AgentOutcome,
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
        outcome.steps,
        outcome.transcript.clone(),
    );
    session.write_to(path)?;
    eprintln!(
        "session: wrote {} messages to {}",
        outcome.transcript.len(),
        path.display()
    );
    Ok(())
}

/// Return Err iff the finish reason should propagate as a non-zero binary
/// exit code so that self-improve.sh's auto-resume gate fires. The
/// transcript has already been saved by the caller before this is called.
fn exit_for_finish(finish: &FinishReason, steps: usize) -> anyhow::Result<()> {
    match finish {
        FinishReason::BudgetExceeded => {
            anyhow::bail!("agent exceeded step budget ({steps})")
        }
        _ => Ok(()),
    }
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
) -> anyhow::Result<()> {
    let seed_len = seed.len();

    // Create SessionWriter if --session is enabled
    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> = if session {
        match SessionWriter::create(&config.workspace, &goal, &config.model, &config.provider_type) {
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

    let on_message: Option<OnMessageFn> = session_writer.clone().map(|sw| {
        Box::new(move |msg: &recursive::message::Message| {
            if let Ok(mut writer) = sw.lock() {
                let _ = writer.append(msg);
            }
        }) as OnMessageFn
    });

    let (mut agent, rx) = build_agent(
        &config,
        max_transcript_chars,
        seed,
        false,
        plan_first,
        mcp_config,
        hook_timing,
        Some(&goal),
        on_message,
    )
    .await?;
    let tools = build_tools(&config).await;
    let tool_specs = tools.specs();
    if !json_mode {
        eprintln!("resuming from {seed_len} seeded message(s)");
    }
    let printer = if json_mode {
        tokio::spawn(stream_events_json(rx))
    } else {
        tokio::spawn(stream_events(rx))
    };
    let outcome = agent.run(goal.clone()).await?;
    drop(agent);
    printer.await.ok();

    // Finalize session writer
    let finish_status = if matches!(outcome.finish, FinishReason::NoMoreToolCalls) {
        "success"
    } else {
        "incomplete"
    };
    if let Some(sw) = session_writer {
        match Arc::into_inner(sw) {
            Some(mutex) => {
                let mut writer = match mutex.lock() {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("session: failed to lock writer: {e}");
                        return Ok(());
                    }
                };
                if let Err(e) = writer.finish(finish_status) {
                    eprintln!("session: failed to finalize: {e}");
                } else {
                    eprintln!("session: saved {} message(s) to {}",
                        writer.message_count(),
                        writer.session_dir().display());
                }
            }
            None => {
                eprintln!("session: writer still has other references; cannot finalize");
            }
        }
    }

    if !json_mode {
        if let Some(ref msg) = outcome.final_message {
            println!("\n=== final ===\n{msg}");
        }
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.total_llm_latency_ms,
            outcome.steps,
            &external_pricing,
        );
        print_finish_note(&outcome.finish);
    }
    if let Some(path) = transcript_out {
        save_transcript(&outcome.transcript, outcome.steps, &config.model, &path)?;
    }
    // Save session file for non-success finishes
    if let Some(path) = session_out {
        let is_success = matches!(outcome.finish, FinishReason::NoMoreToolCalls);
        if !is_success {
            save_session(
                &outcome,
                goal,
                &config.model,
                &config.provider_type,
                &tool_specs,
                &path,
            )?;
        }
    }
    exit_for_finish(&outcome.finish, outcome.steps)
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
    _mcp_config: Option<PathBuf>,
    _external_pricing: Option<HashMap<String, ModelPricing>>,
    hook_timing: bool,
) -> anyhow::Result<()> {
    use std::sync::Mutex;

    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    // Create the shared wakeup slot
    let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));
    let wakeup_slot_clone = wakeup_slot.clone();

    // Build tools with ScheduleWakeup registered
    let mut tools = build_tools(&config).await;
    tools.register_mut(Arc::new(ScheduleWakeup::new(wakeup_slot_clone)));

    // Build the LLM provider
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

    // Build the agent
    let mut builder = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
        .max_steps(config.max_steps);

    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if hook_timing {
        builder = builder.hook(Arc::new(recursive::hooks::ToolTimingHook::new()));
    }
    if stream {
        builder = builder.streaming(true);
    }
    if plan_first {
        builder = builder.planning_mode(PlanningMode::PlanFirst);
    }

    let agent = builder.build()?;

    // Run the loop
    let mut runner = AgentRunner::new(agent);
    let outcomes = runner.run_loop(&goal, &wakeup_slot, None).await?;

    // Print summary
    if json_mode {
        // For JSON mode, just output a simple summary
        let summary: Vec<_> = outcomes
            .iter()
            .map(|o| {
                serde_json::json!({
                    "finish": format!("{:?}", o.finish),
                    "steps": o.steps,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        eprintln!("Loop completed: {} turn(s)", outcomes.len());
    }

    // Use the finish reason from the last outcome
    if let Some(last) = outcomes.last() {
        let _ = exit_for_finish(&last.finish, last.steps);
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
) -> anyhow::Result<()> {
    if let Err(msg) = config.validate_for_agent() {
        eprintln!("{msg}");
        std::process::exit(1);
    }

    // Create SessionWriter if --session is enabled
    let session_writer: Option<Arc<std::sync::Mutex<SessionWriter>>> = if session {
        match SessionWriter::create(&config.workspace, &goal, &config.model, &config.provider_type) {
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

    let on_message: Option<OnMessageFn> = session_writer.clone().map(|sw| {
        Box::new(move |msg: &recursive::message::Message| {
            if let Ok(mut writer) = sw.lock() {
                let _ = writer.append(msg);
            }
        }) as OnMessageFn
    });

    let (mut agent, rx) = build_agent(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        plan_first,
        mcp_config,
        hook_timing,
        None,
        on_message,
    )
    .await?;
    let tools = build_tools(&config).await;
    let tool_specs = tools.specs();
    let printer = if json_mode {
        tokio::spawn(stream_events_json(rx))
    } else {
        tokio::spawn(stream_events(rx))
    };
    let outcome = loop {
        let outcome = agent.run(goal.clone()).await?;
        if !matches!(outcome.finish, FinishReason::PlanPending) {
            break outcome;
        }
        let plan_text = outcome.final_message.as_deref().unwrap_or("(no plan)");
        eprintln!("\n=== Proposed Plan ===\n{plan_text}");
        eprint!("Confirm plan? [Y/n] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            agent.confirm_plan();
        } else {
            agent.reject_plan("User rejected the plan");
            break outcome;
        }
    };
    drop(agent);
    printer.await.ok();
    if !json_mode {
        if let Some(ref msg) = outcome.final_message {
            println!("\n=== final ===\n{msg}");
        }
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.total_llm_latency_ms,
            outcome.steps,
            &external_pricing,
        );
        print_finish_note(&outcome.finish);
    }

    // Finalize session writer
    let finish_status = if matches!(outcome.finish, FinishReason::NoMoreToolCalls) {
        "success"
    } else {
        "incomplete"
    };
    if let Some(sw) = session_writer {
        match Arc::into_inner(sw) {
            Some(mutex) => {
                let mut writer = match mutex.lock() {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("session: failed to lock writer: {e}");
                        return Ok(());
                    }
                };
                if let Err(e) = writer.finish(finish_status) {
                    eprintln!("session: failed to finalize: {e}");
                } else {
                    eprintln!("session: saved {} message(s) to {}",
                        writer.message_count(),
                        writer.session_dir().display());
                }
            }
            None => {
                eprintln!("session: writer still has other references; cannot finalize");
            }
        }
    }

    if let Some(path) = transcript_out {
        save_transcript(&outcome.transcript, outcome.steps, &config.model, &path)?;
    }
    // Save session file for non-success finishes
    if let Some(path) = session_out {
        let is_success = matches!(outcome.finish, FinishReason::NoMoreToolCalls);
        if !is_success {
            save_session(
                &outcome,
                goal,
                &config.model,
                &config.provider_type,
                &tool_specs,
                &path,
            )?;
        }
    }
    exit_for_finish(&outcome.finish, outcome.steps)
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

    // Build agent ONCE — MCP servers are spawned here and stay alive.
    let (mut agent, rx) = build_agent(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        plan_first,
        mcp_config,
        hook_timing,
        None,
        None,
    )
    .await?;
    // Drop the initial rx (no events to print before first turn)
    drop(rx);

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
            agent.set_transcript(Vec::new());
            total_turns = 0;
            if !json_mode {
                eprintln!("(conversation cleared)");
            }
            continue;
        }

        // Create a fresh events channel for this turn
        let (tx, rx) = mpsc::unbounded_channel();
        agent.set_events(Some(tx));

        let printer = if json_mode {
            tokio::spawn(stream_events_json(rx))
        } else {
            tokio::spawn(stream_events_repl(rx))
        };

        match agent.run(goal.to_string()).await {
            Ok(outcome) => {
                // Close the events channel so the printer task finishes
                agent.set_events(None);
                printer.await.ok();

                if !json_mode {
                    print_usage(
                        outcome.total_usage,
                        &config.model,
                        outcome.total_llm_latency_ms,
                        outcome.steps,
                        &external_pricing,
                    );
                    print_finish_note(&outcome.finish);
                }

                // Restore transcript for next turn (run() takes it via mem::take)
                agent.set_transcript(outcome.transcript);
                total_turns += 1;
            }
            Err(e) => {
                agent.set_events(None);
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

async fn stream_events(mut rx: mpsc::UnboundedReceiver<StepEvent>) {
    while let Some(ev) = rx.recv().await {
        #[allow(clippy::collapsible_match)]
        match ev {
            StepEvent::AssistantText { text, step } => {
                if !text.trim().is_empty() {
                    println!("[step {step}] assistant: {text}");
                }
            }
            StepEvent::ToolCall { call, step } => {
                println!("[step {step}] -> {} {}", call.name, call.arguments);
            }
            StepEvent::ToolResult {
                name, output, step, ..
            } => {
                let preview = if output.len() > 800 {
                    format!("{}\n...[truncated]", &output[..800])
                } else {
                    output
                };
                println!("[step {step}] <- {name}\n{preview}");
            }
            StepEvent::Finished { reason, steps } => {
                println!("[done after {steps} steps] reason: {:?}", reason);
            }
            StepEvent::Usage { .. } => {}
            StepEvent::Latency { step, llm_ms } => {
                println!("[step {step}] llm latency: {llm_ms}ms");
            }
            StepEvent::PartialToken { .. } => {}
            StepEvent::Compacted {
                removed,
                kept,
                summary_chars,
                step,
            } => {
                println!(
                    "[step {step}] compacted {removed} msgs -> {kept} kept + {summary_chars}-char summary"
                );
            }
            StepEvent::PlanProposed { plan_text, .. } => {
                println!("[plan] proposed: {plan_text}");
            }
            StepEvent::PlanConfirmed => {
                println!("[plan] confirmed");
            }
            StepEvent::PlanRejected { reason } => {
                println!("[plan] rejected: {reason}");
            }
            _ => {}
        }
    }
}

/// REPL-specific event handler: clean output without step prefixes on assistant text.
/// Tool calls are shown briefly; assistant text is printed directly.
async fn stream_events_repl(mut rx: mpsc::UnboundedReceiver<StepEvent>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            StepEvent::AssistantText { ref text, .. } if !text.trim().is_empty() => {
                println!("{text}");
            }
            StepEvent::AssistantText { .. } => {}
            StepEvent::ToolCall { call, .. } => {
                eprintln!("  ↳ {}", call.name);
            }
            StepEvent::ToolResult { .. } => {}
            StepEvent::Finished { .. } => {}
            StepEvent::Usage { .. } => {}
            StepEvent::Latency { .. } => {}
            StepEvent::PartialToken { .. } => {}
            StepEvent::Compacted { .. } => {}
            _ => {}
        }
    }
}

async fn stream_events_json(mut rx: mpsc::UnboundedReceiver<StepEvent>) {
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
    /// Smoke test for `build_agent` across the matrix of stream flag and
    /// provider selector. Consolidated into ONE test per AGENTS.md guidance
    /// because the anthropic branch reads `RECURSIVE_PROVIDER_TYPE` from the
    /// process env — running it in parallel with other build-agent tests
    /// would race on that global. Asserts:
    ///   - stream=false / openai (default)  → ok (regresses 92d257e bug)
    ///   - stream=true  / openai            → ok (regresses streaming-merge bug)
    ///   - stream=false / anthropic         → ok (g47 dogfood)
    /// The anthropic branch sets+restores the env var to keep the test
    /// hermetic for any tests that come after it.
    #[tokio::test]
    async fn build_agent_construction_smoke() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = dummy_config(tmp.path());

        let r1 = build_agent(
            &cfg,
            None,
            Vec::new(),
            /* stream */ false,
            false,
            None,
            false,
            None,
            None,
        )
        .await;
        assert!(r1.is_ok(), "openai/stream=false: must not panic or fail");

        let r2 = build_agent(
            &cfg,
            None,
            Vec::new(),
            /* stream */ true,
            false,
            None,
            false,
            None,
            None,
        )
        .await;
        assert!(r2.is_ok(), "openai/stream=true: must not panic or fail");

        let original = std::env::var("RECURSIVE_PROVIDER_TYPE").ok();
        std::env::set_var("RECURSIVE_PROVIDER_TYPE", "anthropic");
        let mut cfg_anthropic = dummy_config(tmp.path());
        cfg_anthropic.provider_type = "anthropic".into();
        let r3 = build_agent(
            &cfg_anthropic,
            None,
            Vec::new(),
            false,
            false,
            None,
            false,
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

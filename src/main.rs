//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::Level;

use recursive::config::load_project_context;
use recursive::mcp::{load_mcp_config, McpClient, McpServer, McpTool};
use recursive::skills::{discover_skills, skill_index, Skill};
use recursive::{
    config::Config,
    llm::{pricing_for, LlmProvider, OpenAiProvider, TokenUsage},
    tools::memory::memory_summary,
    tools::{
        ApplyPatch, Forget, ListDir, LoadSkill, ReadFile, Recall, Remember, RunShell, SearchFiles,
        SubAgent, WebFetch, WriteFile,
    },
    Agent, FinishReason, RetryPolicy, StepEvent, ToolRegistry, TranscriptFile,
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

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the agent once with the given goal (concatenated).
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        goal: Vec<String>,
    },
    /// Read goals from stdin, one per line.
    Repl,
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
    if let Some(p) = cli.system_prompt_file {
        config.system_prompt = std::fs::read_to_string(&p)
            .with_context(|| format!("reading system prompt: {}", p.display()))?;
    }

    match cli.cmd {
        Cmd::Tools => {
            let tools = build_tools(&config).await;
            let specs = tools.specs();
            println!("{}", serde_json::to_string_pretty(&specs)?);
            Ok(())
        }
        Cmd::Run { goal } => {
            run_once(
                config,
                goal.join(" "),
                cli.max_transcript_chars,
                cli.transcript_out,
                cli.json,
                cli.stream,
                cli.mcp_config,
            )
            .await
        }
        Cmd::Repl => repl(config, cli.max_transcript_chars, cli.json, cli.mcp_config).await,
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
                            "--resume-from {n} exceeds saved transcript length ({})",
                            file.messages().len()
                        )
                    })?;
                    run_resumed(
                        config,
                        seed.to_vec(),
                        goal.join(" "),
                        cli.max_transcript_chars,
                        cli.transcript_out,
                        cli.json,
                        cli.mcp_config,
                    )
                    .await
                }
            }
        }
    }
}

fn init_logging(level: &str) -> anyhow::Result<()> {
    let lvl: Level = level.parse().context("invalid log level")?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(lvl.to_string()));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
    Ok(())
}

/// Build the tool registry, optionally registering MCP tools from a config file.
async fn build_tools(config: &Config) -> ToolRegistry {
    let root = &config.workspace;
    let mut registry = ToolRegistry::new()
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ApplyPatch::new(root)))
        .register(Arc::new(ListDir::new(root)))
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(root)))
        .register(Arc::new(WebFetch::new()));
    registry = registry
        .register(Arc::new(Remember::new(root)))
        .register(Arc::new(Recall::new(root)))
        .register(Arc::new(Forget::new(root)));
    let skills = discover_loaded_skills(config);
    if !skills.is_empty() {
        registry = registry.register(Arc::new(LoadSkill::new(skills)));
    }
    registry
}

/// Register MCP tools from a config file into the registry.
async fn register_mcp_tools(registry: &mut ToolRegistry, mcp_config_path: Option<PathBuf>) {
    let Some(path) = mcp_config_path else {
        return;
    };
    if !path.exists() {
        eprintln!("warning: MCP config file not found: {}", path.display());
        return;
    }
    let servers = match load_mcp_config(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: failed to load MCP config: {e}");
            return;
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
async fn build_agent(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
    stream: bool,
    mcp_config: Option<PathBuf>,
) -> anyhow::Result<(Agent, mpsc::UnboundedReceiver<StepEvent>)> {
    let api_key = config.require_api_key()?;
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
    let provider: Arc<dyn LlmProvider> = Arc::new(openai);
    let mut tools = build_tools(config).await;
    register_mcp_tools(&mut tools, mcp_config).await;

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
        );
        tools = tools.register(Arc::new(sub));
    }

    let skills = discover_loaded_skills(config);

    // Load project context from AGENTS.md if present
    let project_context = load_project_context(&config.workspace);
    let system_prompt = match (&project_context, skills.is_empty()) {
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
    // Inject memory summary (top 5 most recent notes) into the system prompt
    let memory_block = memory_summary(&config.workspace, 5);
    let system_prompt = if memory_block.is_empty() {
        system_prompt
    } else {
        format!("{}\n\n{}", system_prompt, memory_block)
    };

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
    builder = builder.streaming(stream);
    let agent = builder.build()?;
    Ok((agent, rx))
}

fn print_usage(usage: TokenUsage, model: &str, total_llm_latency_ms: u64, steps: usize) {
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
        if let Some(pricing) = pricing_for(model) {
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

async fn run_resumed(
    config: Config,
    seed: Vec<recursive::message::Message>,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    json_mode: bool,
    mcp_config: Option<PathBuf>,
) -> anyhow::Result<()> {
    let seed_len = seed.len();
    let (mut agent, rx) =
        build_agent(&config, max_transcript_chars, seed, false, mcp_config).await?;
    if !json_mode {
        eprintln!("resuming from {seed_len} seeded message(s)");
    }
    let printer = if json_mode {
        tokio::spawn(stream_events_json(rx))
    } else {
        tokio::spawn(stream_events(rx))
    };
    let outcome = agent.run(goal).await?;
    drop(agent);
    printer.await.ok();
    if !json_mode {
        if let Some(msg) = outcome.final_message {
            println!("\n=== final ===\n{msg}");
        }
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.total_llm_latency_ms,
            outcome.steps,
        );
        print_finish_note(&outcome.finish);
    }
    if let Some(path) = transcript_out {
        save_transcript(&outcome.transcript, outcome.steps, &config.model, &path)?;
    }
    exit_for_finish(&outcome.finish, outcome.steps)
}

async fn run_once(
    config: Config,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    json_mode: bool,
    stream: bool,
    mcp_config: Option<PathBuf>,
) -> anyhow::Result<()> {
    let (mut agent, rx) = build_agent(
        &config,
        max_transcript_chars,
        Vec::new(),
        stream,
        mcp_config,
    )
    .await?;
    let printer = if json_mode {
        tokio::spawn(stream_events_json(rx))
    } else {
        tokio::spawn(stream_events(rx))
    };
    let outcome = agent.run(goal).await?;
    drop(agent);
    printer.await.ok();
    if !json_mode {
        if let Some(msg) = outcome.final_message {
            println!("\n=== final ===\n{msg}");
        }
        print_usage(
            outcome.total_usage,
            &config.model,
            outcome.total_llm_latency_ms,
            outcome.steps,
        );
        print_finish_note(&outcome.finish);
    }

    if let Some(path) = transcript_out {
        save_transcript(&outcome.transcript, outcome.steps, &config.model, &path)?;
    }
    exit_for_finish(&outcome.finish, outcome.steps)
}

async fn repl(
    config: Config,
    max_transcript_chars: Option<usize>,
    json_mode: bool,
    mcp_config: Option<PathBuf>,
) -> anyhow::Result<()> {
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
        let (mut agent, rx) = build_agent(
            &config,
            max_transcript_chars,
            Vec::new(),
            false,
            mcp_config.clone(),
        )
        .await?;
        let printer = if json_mode {
            tokio::spawn(stream_events_json(rx))
        } else {
            tokio::spawn(stream_events(rx))
        };
        match agent.run(goal.to_string()).await {
            Ok(outcome) => {
                drop(agent);
                printer.await.ok();
                if !json_mode {
                    if let Some(msg) = outcome.final_message {
                        println!("\n=== final ===\n{msg}\n");
                    }
                    print_usage(
                        outcome.total_usage,
                        &config.model,
                        outcome.total_llm_latency_ms,
                        outcome.steps,
                    );
                    print_finish_note(&outcome.finish);
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
            }
        }
    }
    Ok(())
}

async fn stream_events(mut rx: mpsc::UnboundedReceiver<StepEvent>) {
    while let Some(ev) = rx.recv().await {
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
            StepEvent::Usage { .. } => {
                // Usage events are already accumulated and printed at end of run
            }
            StepEvent::Latency { step, llm_ms } => {
                println!("[step {step}] llm latency: {llm_ms}ms");
            }
            StepEvent::PartialToken { .. } => {
                // Deltas are forwarded through the events channel.
                // Print them live on stderr (no newline, tokens accumulate).
                // The `text` field is destructured above; we use `..` here
                // because the handler in `stream_events` is for non-streaming
                // mode. In streaming mode, deltas are printed by the agent
                // loop's spawned task.
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_config(tmp: &std::path::Path) -> Config {
        Config {
            workspace: tmp.to_path_buf(),
            api_base: "https://example.invalid/v1".into(),
            api_key: Some("dummy-test-key".into()),
            model: "test-model".into(),
            max_steps: 1,
            temperature: 0.0,
            system_prompt: "test".into(),
            retry_max: 0,
            retry_initial_backoff_secs: 1,
            retry_max_backoff_secs: 1,
            shell_timeout_secs: 5,
        }
    }

    // Regression for the streaming-SSE merge bug (commit 92d257e) where
    // the non-streaming code path called `bool::then(...).unwrap()` and
    // panicked because `then(false)` returns None. This made every
    // `recursive run` (default: stream=false) panic at startup, which
    // in turn broke all parallel-self-improve.sh launches in batch 13.
    #[tokio::test]
    async fn build_agent_does_not_panic_without_stream() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = dummy_config(tmp.path());
        let built = build_agent(&cfg, None, Vec::new(), /* stream */ false, None).await;
        assert!(built.is_ok(), "construction must not panic or fail");
    }

    #[tokio::test]
    async fn build_agent_does_not_panic_with_stream() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = dummy_config(tmp.path());
        let built = build_agent(&cfg, None, Vec::new(), /* stream */ true, None).await;
        assert!(built.is_ok(), "construction must not panic or fail");
    }
}

//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::Level;

use recursive::{
    config::Config,
    llm::{pricing_for, LlmProvider, OpenAiProvider, TokenUsage},
    tools::{ApplyPatch, ListDir, ReadFile, RunShell, SearchFiles, WriteFile},
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
            let tools = build_tools(&config);
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
            )
            .await
        }
        Cmd::Repl => repl(config, cli.max_transcript_chars, cli.json).await,
        Cmd::Replay {
            path,
            resume_from,
            goal,
            tail,
        } => {
            let file = recursive::TranscriptFile::read_from(&path)?;
            match resume_from {
                None => {
                    // If --tail is provided without --resume-from, use pretty_tail
                    if let Some(n) = tail {
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

fn build_tools(config: &Config) -> ToolRegistry {
    let root = &config.workspace;
    ToolRegistry::new()
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ApplyPatch::new(root)))
        .register(Arc::new(ListDir::new(root)))
        .register(Arc::new(
            RunShell::new(root).with_timeout(Duration::from_secs(config.shell_timeout_secs)),
        ))
        .register(Arc::new(SearchFiles::new(root)))
}

fn build_agent(
    config: &Config,
    max_transcript_chars: Option<usize>,
) -> anyhow::Result<(Agent, mpsc::UnboundedReceiver<StepEvent>)> {
    build_agent_seeded(config, max_transcript_chars, Vec::new())
}

fn build_agent_seeded(
    config: &Config,
    max_transcript_chars: Option<usize>,
    seed: Vec<recursive::message::Message>,
) -> anyhow::Result<(Agent, mpsc::UnboundedReceiver<StepEvent>)> {
    let api_key = config.require_api_key()?;
    let retry = RetryPolicy {
        max_retries: config.retry_max,
        initial_backoff: Duration::from_secs(config.retry_initial_backoff_secs),
        max_backoff: Duration::from_secs(config.retry_max_backoff_secs),
    };
    let provider: Arc<dyn LlmProvider> = Arc::new(
        OpenAiProvider::new(&config.api_base, api_key, &config.model)
            .with_temperature(config.temperature)
            .with_retry_policy(retry),
    );
    let tools = build_tools(config);
    let (tx, rx) = mpsc::unbounded_channel();
    let mut builder = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
        .max_steps(config.max_steps)
        .events(tx);
    if let Some(n) = max_transcript_chars {
        builder = builder.max_transcript_chars(n);
    }
    if !seed.is_empty() {
        builder = builder.seed_transcript(seed);
    }
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

async fn run_resumed(
    config: Config,
    seed: Vec<recursive::message::Message>,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let seed_len = seed.len();
    let (mut agent, rx) = build_agent_seeded(&config, max_transcript_chars, seed)?;
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
        let file = TranscriptFile::new(
            outcome.transcript.clone(),
            outcome.steps,
            Some(config.model.clone()),
        );
        file.write_to(&path)?;
        eprintln!(
            "transcript: wrote {} messages to {}",
            outcome.transcript.len(),
            path.display()
        );
    }
    Ok(())
}

async fn run_once(
    config: Config,
    goal: String,
    max_transcript_chars: Option<usize>,
    transcript_out: Option<PathBuf>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let (mut agent, rx) = build_agent(&config, max_transcript_chars)?;
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
        let file = TranscriptFile::new(
            outcome.transcript.clone(),
            outcome.steps,
            Some(config.model.clone()),
        );
        file.write_to(&path)?;
        eprintln!(
            "transcript: wrote {} messages to {}",
            outcome.transcript.len(),
            path.display()
        );
    }
    Ok(())
}

async fn repl(
    config: Config,
    max_transcript_chars: Option<usize>,
    json_mode: bool,
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
        let (mut agent, rx) = build_agent(&config, max_transcript_chars)?;
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

//! `recursive` CLI: a thin shell around the kernel.
//!
//! Subcommands:
//!   - `run <goal...>`: run the agent once with the given goal.
//!   - `repl`:          interactive loop, one goal per line.
//!   - `tools`:         print the registered tool specs as JSON.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::Level;

use recursive::{
    config::Config,
    llm::{LlmProvider, OpenAiProvider},
    tools::{CountLines, ListDir, ReadFile, RunShell, WriteFile},
    Agent, StepEvent, ToolRegistry,
};

#[derive(Parser, Debug)]
#[command(name = "recursive", version, about = "A minimal self-improving coding agent")]
struct Cli {
    /// Workspace root the agent can read/write within.
    #[arg(long, env = "RECURSIVE_WORKSPACE")]
    workspace: Option<PathBuf>,

    /// Maximum agent loop iterations per goal.
    #[arg(long, env = "RECURSIVE_MAX_STEPS")]
    max_steps: Option<usize>,

    /// Path to a system prompt file (overrides default).
    #[arg(long, env = "RECURSIVE_SYSTEM_PROMPT_FILE")]
    system_prompt_file: Option<PathBuf>,

    /// Log level: error|warn|info|debug|trace.
    #[arg(long, default_value = "info")]
    log: String,

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
            let tools = build_tools(&config.workspace);
            let specs = tools.specs();
            println!("{}", serde_json::to_string_pretty(&specs)?);
            Ok(())
        }
        Cmd::Run { goal } => run_once(config, goal.join(" ")).await,
        Cmd::Repl => repl(config).await,
    }
}

fn init_logging(level: &str) -> anyhow::Result<()> {
    let lvl: Level = level.parse().context("invalid log level")?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(lvl.to_string()));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).compact().init();
    Ok(())
}

fn build_tools(root: &std::path::Path) -> ToolRegistry {
    ToolRegistry::new()
        .register(Arc::new(ReadFile::new(root)))
        .register(Arc::new(WriteFile::new(root)))
        .register(Arc::new(ListDir::new(root)))
        .register(Arc::new(CountLines::new(root)))
        .register(Arc::new(RunShell::new(root)))
}

fn build_agent(config: &Config) -> anyhow::Result<(Agent, mpsc::UnboundedReceiver<StepEvent>)> {
    let api_key = config.require_api_key()?;
    let provider: Arc<dyn LlmProvider> = Arc::new(
        OpenAiProvider::new(&config.api_base, api_key, &config.model)
            .with_temperature(config.temperature),
    );
    let tools = build_tools(&config.workspace);
    let (tx, rx) = mpsc::unbounded_channel();
    let agent = Agent::builder()
        .llm(provider)
        .tools(tools)
        .system_prompt(&config.system_prompt)
        .max_steps(config.max_steps)
        .events(tx)
        .build()?;
    Ok((agent, rx))
}

async fn run_once(config: Config, goal: String) -> anyhow::Result<()> {
    let (mut agent, rx) = build_agent(&config)?;
    let printer = tokio::spawn(stream_events(rx));
    let outcome = agent.run(goal).await?;
    drop(agent);
    printer.await.ok();
    if let Some(msg) = outcome.final_message {
        println!("\n=== final ===\n{msg}");
    }
    Ok(())
}

async fn repl(config: Config) -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    loop {
        eprint!("recursive> ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let Some(line) = lines.next_line().await? else { break };
        let goal = line.trim();
        if goal.is_empty() {
            continue;
        }
        if matches!(goal, ":q" | ":quit" | "exit") {
            break;
        }
        let (mut agent, rx) = build_agent(&config)?;
        let printer = tokio::spawn(stream_events(rx));
        match agent.run(goal.to_string()).await {
            Ok(outcome) => {
                drop(agent);
                printer.await.ok();
                if let Some(msg) = outcome.final_message {
                    println!("\n=== final ===\n{msg}\n");
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
            StepEvent::ToolResult { name, output, step, .. } => {
                let preview = if output.len() > 800 { format!("{}\n...[truncated]", &output[..800]) } else { output };
                println!("[step {step}] <- {name}\n{preview}");
            }
            StepEvent::Finished { reason, steps } => {
                println!("[done after {steps} steps] reason: {:?}", reason);
            }
        }
    }
}

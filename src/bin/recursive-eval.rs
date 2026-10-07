//! `recursive-eval` — the preset benchmark harness (issue #129).
//!
//! One command runs the fixed task set under every preset and writes the raw
//! per-run records (JSONL) plus a markdown comparison:
//!
//! ```text
//! cargo run --bin recursive-eval -- run
//! cargo run --bin recursive-eval -- run --mode live --repeat 3
//! cargo run --bin recursive-eval -- report eval/results/<file>.jsonl
//! ```
//!
//! `run` defaults to replay mode: a scripted provider, no API key, reproducible
//! tokens and success. `--mode live` uses the configured provider and is the
//! quality measurement; see `eval/README.md`.

#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use recursive::config::Config;
use recursive::eval::{self, Mode, Sample};
use recursive::preset::{self, AgentPreset};

const USAGE: &str = "\
recursive-eval — preset 增益量化基准 (issue #129)

USAGE:
    recursive-eval list
    recursive-eval run [options]
    recursive-eval report <jsonl> [--out <file>]

RUN OPTIONS:
    --mode <replay|live>     replay (default, deterministic, no key) | live
    --presets <a,b>          preset ids (default: every built-in preset)
    --tasks <id,id>          task ids (default: the whole fixed task set)
    --repeat <n>             run the suite n times (default: 1)
    --out <file>             JSONL destination (default: eval/results/<ts>-<mode>.jsonl)
    --report <file>          markdown destination (default: eval/report.md)
    --workdir <dir>          scratch workspace root (default: a temp dir, removed after)
    --api-key <key>          provider key for --mode live (default: config / RECURSIVE_API_KEY)
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("recursive-eval: {message}");
            ExitCode::from(2)
        }
    }
}

async fn dispatch(args: Vec<String>) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("list") => {
            list();
            Ok(())
        }
        Some("report") => cmd_report(&args[1..]),
        Some("run") => cmd_run(&args[1..]).await,
        Some("-h") | Some("--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn list() {
    println!("tasks ({}):", eval::tasks().len());
    for task in eval::tasks() {
        println!(
            "  {:<22} {:<16} {}",
            task.id,
            task.category.as_str(),
            task.goal
        );
    }
    println!("\npresets:");
    for p in preset::builtin() {
        println!("  {:<10} {}", p.id, p.description);
    }
}

#[derive(Debug)]
struct RunOptions {
    mode: Mode,
    presets: Option<Vec<String>>,
    tasks: Option<Vec<String>>,
    repeat: usize,
    out: Option<PathBuf>,
    report: Option<PathBuf>,
    workdir: Option<PathBuf>,
    api_key: Option<String>,
}

fn parse_run_options(args: &[String]) -> Result<RunOptions, String> {
    let mut opts = RunOptions {
        mode: Mode::Replay,
        presets: None,
        tasks: None,
        repeat: 1,
        out: None,
        report: None,
        workdir: None,
        api_key: None,
    };
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| -> Result<&String, String> {
            args.get(i + 1)
                .ok_or_else(|| format!("`{}` needs a value", args[i]))
        };
        match args[i].as_str() {
            "--mode" => {
                opts.mode = match value(i)?.as_str() {
                    "replay" => Mode::Replay,
                    "live" => Mode::Live,
                    other => return Err(format!("unknown mode `{other}` (replay|live)")),
                };
                i += 2;
            }
            "--presets" => {
                opts.presets = Some(split_list(value(i)?));
                i += 2;
            }
            "--tasks" => {
                opts.tasks = Some(split_list(value(i)?));
                i += 2;
            }
            "--repeat" => {
                opts.repeat = value(i)?
                    .parse::<usize>()
                    .map_err(|e| format!("--repeat: {e}"))?
                    .max(1);
                i += 2;
            }
            "--out" => {
                opts.out = Some(PathBuf::from(value(i)?));
                i += 2;
            }
            "--report" => {
                opts.report = Some(PathBuf::from(value(i)?));
                i += 2;
            }
            "--workdir" => {
                opts.workdir = Some(PathBuf::from(value(i)?));
                i += 2;
            }
            "--api-key" => {
                opts.api_key = Some(value(i)?.clone());
                i += 2;
            }
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }
    Ok(opts)
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn resolve_presets(ids: &Option<Vec<String>>) -> Result<Vec<&'static AgentPreset>, String> {
    match ids {
        None => Ok(preset::builtin().to_vec()),
        Some(ids) => ids
            .iter()
            .map(|id| {
                preset::find(id).ok_or_else(|| {
                    format!(
                        "unknown preset `{id}` (known: {})",
                        preset::builtin_ids().join(", ")
                    )
                })
            })
            .collect(),
    }
}

fn resolve_tasks(ids: &Option<Vec<String>>) -> Result<Vec<&'static eval::EvalTask>, String> {
    match ids {
        None => Ok(eval::tasks().iter().collect()),
        Some(ids) => ids
            .iter()
            .map(|id| eval::task(id).ok_or_else(|| format!("unknown task `{id}`")))
            .collect(),
    }
}

async fn cmd_run(args: &[String]) -> Result<(), String> {
    let opts = parse_run_options(args)?;
    let mut config = Config::from_env().map_err(|e| format!("config: {e}"))?;
    if let Ok(cwd) = std::env::current_dir() {
        config.workspace = cwd;
    }
    let presets = resolve_presets(&opts.presets)?;
    let tasks = resolve_tasks(&opts.tasks)?;

    let api_key = match opts.mode {
        Mode::Replay => None,
        Mode::Live => Some(
            opts.api_key
                .clone()
                .or_else(|| config.api_key.clone())
                .ok_or_else(|| {
                    "live mode needs an API key (--api-key, config, or RECURSIVE_API_KEY)"
                        .to_string()
                })?,
        ),
    };

    let workdir = opts.workdir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("recursive-eval-{}", std::process::id()))
    });
    std::fs::create_dir_all(&workdir).map_err(|e| format!("workdir {}: {e}", workdir.display()))?;

    let mut samples: Vec<Sample> = Vec::new();
    for round in 0..opts.repeat {
        for task in &tasks {
            let round_dir = workdir.join(format!("round-{round}"));
            let batch = eval::run_task_across_presets(
                &config,
                task,
                &presets,
                opts.mode,
                &round_dir,
                api_key.as_deref(),
            )
            .await;
            for sample in &batch {
                eprintln!(
                    "[{:<9} {:<22}] {:<4} {} in={} out={} wall={}ms turns={} tools={}",
                    sample.preset,
                    sample.task_id,
                    if sample.success { "pass" } else { "FAIL" },
                    sample.mode,
                    sample.input_tokens,
                    sample.output_tokens,
                    sample.wall_ms,
                    sample.turns,
                    sample.tool_calls,
                );
            }
            samples.extend(batch);
        }
    }

    let out = opts
        .out
        .unwrap_or_else(|| PathBuf::from(default_jsonl_path(opts.mode)));
    let report = opts
        .report
        .unwrap_or_else(|| PathBuf::from("eval/report.md"));

    write_file(&out, &eval::to_jsonl(&samples).map_err(|e| e.to_string())?)?;
    write_file(&report, &eval::render_markdown(&samples))?;

    if opts.workdir.is_none() {
        let _ = std::fs::remove_dir_all(&workdir);
    }

    println!("wrote {} samples to {}", samples.len(), out.display());
    println!("wrote report to {}", report.display());
    Ok(())
}

fn default_jsonl_path(mode: Mode) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    format!("eval/results/{stamp}-{}.jsonl", mode.as_str())
}

fn cmd_report(args: &[String]) -> Result<(), String> {
    let mut input: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out = Some(PathBuf::from(
                    args.get(i + 1).ok_or("`--out` needs a value")?,
                ));
                i += 2;
            }
            path => {
                input = Some(PathBuf::from(path));
                i += 1;
            }
        }
    }
    let input = input.ok_or_else(|| format!("report needs a JSONL path\n\n{USAGE}"))?;
    let text =
        std::fs::read_to_string(&input).map_err(|e| format!("read {}: {e}", input.display()))?;
    let samples = eval::parse_jsonl(&text).map_err(|e| e.to_string())?;
    let markdown = eval::render_markdown(&samples);
    match out {
        Some(path) => {
            write_file(&path, &markdown)?;
            println!("wrote report to {}", path.display());
        }
        None => println!("{markdown}"),
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()))
}

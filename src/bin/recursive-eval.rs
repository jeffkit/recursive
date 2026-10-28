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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn parse_ok(list: &[&str]) -> RunOptions {
        match parse_run_options(&argv(list)) {
            Ok(opts) => opts,
            Err(e) => panic!("parse_run_options({list:?}) unexpectedly failed: {e}"),
        }
    }

    fn parse_err(list: &[&str]) -> String {
        match parse_run_options(&argv(list)) {
            Ok(_) => panic!("parse_run_options({list:?}) unexpectedly succeeded"),
            Err(e) => e,
        }
    }

    /// Kills the `while i < args.len()` boundary mutants (`<=` panics past the
    /// end, `>` never enters the loop) and the `args.get(i + 1)` mutants
    /// (`-` underflows / returns the flag itself, `*` re-reads the flag).
    ///
    /// Each `i += 2` arm is killed at two offsets: with the flag first
    /// (`-= underflows to a panic in debug builds; any other placement
    /// rewinds into an infinite loop that only the mutation run's timeout
    /// can catch), and with the flag at offset 4 (`*= jumps over the next
    /// option pair, observable via the skipped flag's field staying None).
    #[test]
    fn parse_run_options_happy_path_parses_every_flag() {
        // --mode first: the `-= on i` mutant underflows at offset 0.
        let opts = parse_ok(&[
            "--mode",
            "live",
            "--tasks",
            "t1,t2",
            "--report",
            "r.md",
            "--presets",
            " alpha , beta ",
        ]);
        assert!(matches!(opts.mode, Mode::Live));
        assert_eq!(
            opts.tasks.as_deref(),
            Some(&["t1".to_string(), "t2".to_string()][..])
        );
        assert_eq!(opts.report.as_deref(), Some(Path::new("r.md")));
        assert_eq!(
            opts.presets.as_deref(),
            Some(&["alpha".to_string(), "beta".to_string()][..])
        );
        assert_eq!(opts.repeat, 1);
        assert_eq!(opts.out, None);

        // --mode at offset 4: the `*= on i` mutant jumps from 4 to 8 and
        // skips ["--report", "r.md"], so report must stay None… which this
        // call refutes by parsing it.
        let opts = parse_ok(&[
            "--out", "o.jsonl", "--tasks", "t1", "--mode", "live", "--report", "r.md",
        ]);
        assert!(matches!(opts.mode, Mode::Live));
        assert_eq!(opts.out.as_deref(), Some(Path::new("o.jsonl")));
        assert_eq!(opts.report.as_deref(), Some(Path::new("r.md")));

        // --presets at offset 4: the `*= on i` mutant skips ["--out", …].
        let opts = parse_ok(&[
            "--tasks",
            "t1",
            "--report",
            "r.md",
            "--presets",
            " alpha , beta ",
            "--out",
            "o.jsonl",
        ]);
        assert_eq!(
            opts.presets.as_deref(),
            Some(&["alpha".to_string(), "beta".to_string()][..])
        );
        assert_eq!(opts.out.as_deref(), Some(Path::new("o.jsonl")));

        // --presets first: the `-= on i` mutant underflows at offset 0.
        let opts = parse_ok(&["--presets", "alpha", "--mode", "live"]);
        assert_eq!(opts.presets.as_deref(), Some(&["alpha".to_string()][..]));
        assert!(matches!(opts.mode, Mode::Live));
    }

    #[test]
    fn parse_run_options_rejects_bad_values() {
        let e = parse_err(&["--mode", "bogus"]);
        assert!(e.contains("unknown mode"), "{e}");
        let e = parse_err(&["--out"]);
        assert!(e.contains("needs a value"), "{e}");
        let e = parse_err(&["--frobnicate"]);
        assert!(e.contains("unknown option"), "{e}");
        let e = parse_err(&["--repeat", "notanumber"]);
        assert!(e.contains("--repeat"), "{e}");
    }

    #[test]
    fn repeat_zero_clamps_to_one() {
        let opts = parse_ok(&["--repeat", "0"]);
        assert_eq!(opts.repeat, 1);
        let opts = parse_ok(&["--repeat", "3"]);
        assert_eq!(opts.repeat, 3);
    }

    /// Kills the `replace dispatch with Ok(())` mutant: unknown commands must
    /// be errors, the help path must stay Ok.
    #[test]
    fn dispatch_routes_unknown_command_to_error_and_help_to_ok() {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => panic!("tokio runtime: {e}"),
        };
        let err = match rt.block_on(dispatch(argv(&["definitely-not-a-command"]))) {
            Ok(()) => panic!("unknown command must be an error"),
            Err(e) => e,
        };
        assert!(err.contains("unknown command"), "{err}");
        assert!(rt.block_on(dispatch(Vec::new())).is_ok());
        let err = match rt.block_on(dispatch(argv(&["report"]))) {
            Ok(()) => panic!("report without a path must be an error"),
            Err(e) => e,
        };
        assert!(err.contains("report needs a JSONL path"), "{err}");
    }

    /// Kills the `i += 2` mutants in the two arms the batch-1a pins never
    /// parsed (`--workdir` / `--api-key`) at the same two offsets as the
    /// happy-path pin: flag first (`-=` underflows to a panic; any other
    /// placement rewinds into an infinite loop that only the mutation run's
    /// timeout can catch), and flag at offset 4 (`*=` jumps over the next
    /// option pair, observable via the skipped flag's field staying default).
    #[test]
    fn parse_run_options_workdir_and_api_key_arms_advance_and_store() {
        // --api-key first: the `-= on i` mutant underflows at offset 0.
        let opts = parse_ok(&["--api-key", "sk-test", "--mode", "live"]);
        assert_eq!(opts.api_key.as_deref(), Some("sk-test"));
        assert!(matches!(opts.mode, Mode::Live));

        // --api-key at offset 4: the `*= on i` mutant jumps from 4 to 8 and
        // skips ["--mode", "live"], so mode must stay Replay… which this call
        // refutes by parsing it.
        let opts = parse_ok(&[
            "--out",
            "o.jsonl",
            "--tasks",
            "t1",
            "--api-key",
            "sk",
            "--mode",
            "live",
        ]);
        assert_eq!(opts.api_key.as_deref(), Some("sk"));
        assert!(matches!(opts.mode, Mode::Live));
        assert_eq!(opts.tasks.as_deref(), Some(&["t1".to_string()][..]));

        // --workdir first: the `-= on i` mutant underflows at offset 0.
        let opts = parse_ok(&["--workdir", "wd", "--mode", "live"]);
        assert_eq!(opts.workdir.as_deref(), Some(Path::new("wd")));
        assert!(matches!(opts.mode, Mode::Live));

        // --workdir at offset 4: the `*= on i` mutant jumps from 4 to 8 and
        // skips ["--out", "o.jsonl"].
        let opts = parse_ok(&[
            "--tasks",
            "t1",
            "--report",
            "r.md",
            "--workdir",
            "w",
            "--out",
            "o.jsonl",
        ]);
        assert_eq!(opts.workdir.as_deref(), Some(Path::new("w")));
        assert_eq!(opts.out.as_deref(), Some(Path::new("o.jsonl")));
        assert_eq!(opts.report.as_deref(), Some(Path::new("r.md")));
    }

    /// Kills the `resolve_presets -> Ok(vec![])` mutant: the default must be
    /// the whole built-in catalogue, a named id must resolve to that preset,
    /// and an unknown id must be an error.
    #[test]
    fn resolve_presets_returns_catalogue_and_validates_ids() {
        let none: Option<Vec<String>> = None;
        let all = match resolve_presets(&none) {
            Ok(v) => v,
            Err(e) => panic!("resolve_presets(None): {e}"),
        };
        assert_eq!(all.len(), preset::builtin().len());
        assert!(all.iter().any(|p| p.id == "minimal"));
        assert!(all.iter().any(|p| p.id == "standard"));

        let one = match resolve_presets(&Some(vec!["minimal".to_string()])) {
            Ok(v) => v,
            Err(e) => panic!("resolve_presets(minimal): {e}"),
        };
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "minimal");

        let err = match resolve_presets(&Some(vec!["nope".to_string()])) {
            Ok(_) => panic!("unknown preset must be an error"),
            Err(e) => e,
        };
        assert!(err.contains("unknown preset `nope`"), "{err}");
    }

    /// Kills the `resolve_tasks -> Ok(vec![])` mutant: same catalogue, named
    /// resolution and error triple against the fixed task set.
    #[test]
    fn resolve_tasks_returns_task_set_and_validates_ids() {
        let none: Option<Vec<String>> = None;
        let all = match resolve_tasks(&none) {
            Ok(v) => v,
            Err(e) => panic!("resolve_tasks(None): {e}"),
        };
        assert_eq!(all.len(), eval::tasks().len());
        assert!(all.iter().any(|t| t.id == "single-append-row"));

        let one = match resolve_tasks(&Some(vec!["single-append-row".to_string()])) {
            Ok(v) => v,
            Err(e) => panic!("resolve_tasks(single-append-row): {e}"),
        };
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "single-append-row");

        let err = match resolve_tasks(&Some(vec!["nope".to_string()])) {
            Ok(_) => panic!("unknown task must be an error"),
            Err(e) => e,
        };
        assert!(err.contains("unknown task `nope`"), "{err}");
    }

    /// Kills the `default_jsonl_path -> "xyzzy"` mutant: the default must stay
    /// inside `eval/results/` and carry a `YYYYMMDDTHHMMSSZ` UTC stamp plus
    /// the mode suffix.
    #[test]
    fn default_jsonl_path_carries_results_dir_stamp_and_mode() {
        let replay = default_jsonl_path(Mode::Replay);
        let stem = match replay
            .strip_prefix("eval/results/")
            .and_then(|s| s.strip_suffix("-replay.jsonl"))
        {
            Some(s) => s,
            None => panic!("default replay path has the wrong shape: {replay}"),
        };
        let bytes = stem.as_bytes();
        assert_eq!(bytes.len(), 16, "stamp `{stem}` is not YYYYMMDDTHHMMSSZ");
        assert!(bytes[..8].iter().all(|b| b.is_ascii_digit()), "{stem}");
        assert_eq!(bytes[8], b'T', "{stem}");
        assert!(bytes[9..15].iter().all(|b| b.is_ascii_digit()), "{stem}");
        assert_eq!(bytes[15], b'Z', "{stem}");
        assert!(
            default_jsonl_path(Mode::Live).ends_with("-live.jsonl"),
            "live default path must keep the -live suffix"
        );
    }

    /// Kills both `write_file` mutants: the `Ok(())` body replacement writes
    /// nothing, and deleting the `!` inverts the empty-parent guard so the
    /// `create_dir_all` for a missing parent is skipped and the write fails.
    #[test]
    fn write_file_creates_missing_parent_and_writes_contents() {
        let root = tmp_root("write-file");
        let path = root.join("missing-parent").join("f.txt");
        match write_file(&path, "payload") {
            Ok(()) => {}
            Err(e) => panic!("write_file {:?}: {e}", path),
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => panic!("read back {:?}: {e}", path),
        };
        assert_eq!(text, "payload");
    }

    /// Kills the `cmd_report` loop-bound and advance mutants on the
    /// positional-first shape: `while <` → `>` never parses the input (the
    /// call must be Ok), `+=` → `*=` on the `--out` arm rewinds `i` onto the
    /// out path and overwrites the input with it (the read fails), and
    /// `+=` → `-=` on the positional arm underflows at offset 0.
    #[test]
    fn cmd_report_positional_input_writes_markdown_to_out() {
        let root = tmp_root("report-positional");
        let input = root.join("in.jsonl");
        let out = root.join("rep.md");
        if let Err(e) = std::fs::write(&input, sample_jsonl()) {
            panic!("seed {:?}: {e}", input);
        }
        let args = argv(&[str_of(&input), "--out", str_of(&out)]);
        match cmd_report(&args) {
            Ok(()) => {}
            Err(e) => panic!("cmd_report: {e}"),
        }
        let md = match std::fs::read_to_string(&out) {
            Ok(t) => t,
            Err(e) => panic!("read {:?}: {e}", out),
        };
        assert!(md.contains("# Preset benchmark"), "{md}");
    }

    /// Kills the value-lookup mutants on the `--out`-first shape: the
    /// `args.get(i + 1)` → `get(i - 1)` mutant underflows at offset 0, and so
    /// does the `--out` arm's `+=` → `-=` on `i` itself.
    #[test]
    fn cmd_report_out_flag_first_still_takes_the_next_value() {
        let root = tmp_root("report-out-first");
        let input = root.join("in.jsonl");
        let out = root.join("rep.md");
        if let Err(e) = std::fs::write(&input, sample_jsonl()) {
            panic!("seed {:?}: {e}", input);
        }
        let args = argv(&["--out", str_of(&out), str_of(&input)]);
        match cmd_report(&args) {
            Ok(()) => {}
            Err(e) => panic!("cmd_report: {e}"),
        }
        let md = match std::fs::read_to_string(&out) {
            Ok(t) => t,
            Err(e) => panic!("read {:?}: {e}", out),
        };
        assert!(md.contains("# Preset benchmark"), "{md}");
    }

    /// One replay `Sample` serialized through the real `eval::to_jsonl`, so a
    /// `Sample` field rename breaks these tests at compile time instead of
    /// rotting a hand-written JSON literal.
    fn sample_jsonl() -> String {
        let sample = Sample {
            task_id: "single-append-row".to_string(),
            category: "single-file".to_string(),
            preset: "minimal".to_string(),
            system_prompt_tokens: 1200,
            mode: "replay".to_string(),
            success: true,
            finish_reason: "stop".to_string(),
            input_tokens: 1500,
            output_tokens: 40,
            cache_read_tokens: 0,
            wall_ms: 12,
            turns: 3,
            tool_calls: 2,
            failure: None,
            provider: None,
            model: None,
        };
        match eval::to_jsonl(&[sample]) {
            Ok(text) => text,
            Err(e) => panic!("to_jsonl: {e}"),
        }
    }

    /// Per-test scratch directory under the system temp dir — never the repo
    /// tree, since `cmd_report` / `write_file` write wherever their arguments
    /// point and a relative path would land in the package root.
    fn tmp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("recursive-eval-bin-{}-{tag}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("create temp dir {dir:?}: {e}");
        }
        dir
    }

    fn str_of(path: &Path) -> &str {
        match path.to_str() {
            Some(s) => s,
            None => panic!("non-utf8 path {path:?}"),
        }
    }
}

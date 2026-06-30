//! `tui-pty` CLI — run a TUI binary under a PTY and snapshot its screen.
//!
//! Thin wrapper over the [`tui_pty_harness`] engine. See that crate's docs
//! for the stability poll and the integration-layer role this tool plays
//! in AI-driven TUI testing.
//!
//! # Usage
//!
//! ```text
//! tui-pty run --bin "recursive tui" --keys "hello\r" --wait-ms 1500 --snap text
//! tui-pty run --bin "./my-tui" --keys "hello\r\e" --cols 100 --rows 30 --snap numbered
//! tui-pty run --bin "cargo run -q --bin recursive-tui" --snap json --wait-ms 3000
//! ```
//!
//! `--keys` accepts a small escape grammar: `\r` (CR), `\n` (LF), `\t`
//! (TAB), `\e` or `\x1b` (ESC), `^x` (Ctrl+x, e.g. `^c`). Anything else is
//! typed literally.
//!
//! This is a single-shot runner: one process spawns the TUI, types the
//! script, waits, snapshots, and tears down. A stateful spawn/type/snap
//! daemon (Wrap-style multi-invocation sessions) is a deliberate future
//! extension; the single-shot form already lets an AI observe running
//! state for acceptance tours.

use anyhow::{anyhow, Result};

use tui_pty_harness::{parse_keys, print_snapshot, shell_split, RunSpec, SnapFormat};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty()
        || matches!(
            args.first().map(|s| s.as_str()),
            Some("-h" | "--help" | "help")
        )
    {
        print_usage();
        return Ok(());
    }
    match args[0].as_str() {
        "run" => run_cmd(&args[1..]),
        other => {
            print_usage();
            Err(anyhow!("unknown subcommand: {other}"))
        }
    }
}

fn print_usage() {
    eprintln!(
        "tui-pty — run a TUI under a PTY and snapshot its screen\n\
         \n\
         usage:\n  \
         tui-pty run --bin \"<cmd>\" [--keys \"<script>\"] [--cols N] [--rows N]\n             \
         [--wait-ms N] [--stable-ms N] [--cwd <dir>] [--env K=V]... [--snap text|numbered|json]\n\
         \n\
         keys grammar: \\r \\n \\t \\e(=ESC) ^x(=Ctrl+x); other chars literal\n\
         \n\
         --wait-ms   max wall-clock cap (default 1500)\n\
         --stable-ms snapshot as soon as the screen is unchanged for this many ms (default 120)\n"
    );
}

fn run_cmd(args: &[String]) -> Result<()> {
    let mut bin: Option<String> = None;
    let mut keys = String::new();
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut wait_ms: u64 = 1500;
    let mut stable_ms: u64 = 120;
    let mut cwd: Option<String> = None;
    let mut envs: Vec<(String, String)> = Vec::new();
    let mut snap = SnapFormat::Text;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bin" => {
                i += 1;
                bin = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--bin needs a value"))?,
                );
            }
            "--keys" => {
                i += 1;
                keys = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--keys needs a value"))?;
            }
            "--cols" => {
                i += 1;
                cols = parse_u16(args.get(i), "--cols")?;
            }
            "--rows" => {
                i += 1;
                rows = parse_u16(args.get(i), "--rows")?;
            }
            "--wait-ms" => {
                i += 1;
                wait_ms = parse_u64(args.get(i), "--wait-ms")?;
            }
            "--stable-ms" => {
                i += 1;
                stable_ms = parse_u64(args.get(i), "--stable-ms")?;
            }
            "--cwd" => {
                i += 1;
                cwd = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--cwd needs a value"))?,
                );
            }
            "--env" => {
                i += 1;
                let kv = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow!("--env needs K=V"))?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--env must be K=V, got {kv}"))?;
                envs.push((k.to_string(), v.to_string()));
            }
            "--snap" => {
                i += 1;
                snap = match args.get(i).map(|s| s.as_str()) {
                    Some("text") => SnapFormat::Text,
                    Some("numbered") => SnapFormat::Numbered,
                    Some("json") => SnapFormat::Json,
                    other => {
                        return Err(anyhow!(
                            "unknown --snap {} (use text|numbered|json)",
                            other.unwrap_or("(missing)")
                        ))
                    }
                };
            }
            other => return Err(anyhow!("unknown flag: {other}")),
        }
        i += 1;
    }

    let bin = bin.ok_or_else(|| anyhow!("--bin is required"))?;
    let parts = shell_split(&bin);
    if parts.is_empty() {
        return Err(anyhow!("--bin is empty"));
    }
    let prog = &parts[0];
    let prog_args = &parts[1..];

    let key_bytes = parse_keys(&keys);

    let spec = RunSpec {
        prog,
        args: prog_args,
        keys: &key_bytes,
        cols,
        rows,
        wait_ms,
        stable_ms,
        cwd: cwd.as_deref(),
        envs: &envs,
    };
    let screen = tui_pty_harness::spawn_and_snapshot(&spec)?;
    print_snapshot(&screen, snap);
    Ok(())
}

fn parse_u16(s: Option<&String>, flag: &str) -> Result<u16> {
    s.ok_or_else(|| anyhow!("{flag} needs a value"))?
        .parse::<u16>()
        .map_err(|e| anyhow!("{flag} invalid: {e}"))
}

fn parse_u64(s: Option<&String>, flag: &str) -> Result<u64> {
    s.ok_or_else(|| anyhow!("{flag} needs a value"))?
        .parse::<u64>()
        .map_err(|e| anyhow!("{flag} invalid: {e}"))
}

//! `tui-pty` — run a TUI binary under a PTY and snapshot its screen.
//!
//! The integration-layer "eyes" for AI-driven TUI testing (stage 4/5).
//! Where the in-process `recursive-tui` harness covers logic + rendering,
//! this binary runs the **real** executable in a pseudo-terminal, parses
//! its ANSI output with a real terminal state model (`vt100`), and prints
//! the resulting screen — exactly what a user's terminal would show. This
//! is the layer `cargo-mutants` flagged as untested in `recursive-tui`'s
//! `lib.rs` (raw mode, alternate screen, mouse): it only exists behind a
//! real PTY.
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

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use vt100::Parser as VtParser;

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
         [--wait-ms N] [--cwd <dir>] [--env K=V]... [--snap text|numbered|json]\n\
         \n\
         keys grammar: \\r \\n \\t \\e(=ESC) ^x(=Ctrl+x); other chars literal\n"
    );
}

fn run_cmd(args: &[String]) -> Result<()> {
    let mut bin: Option<String> = None;
    let mut keys = String::new();
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut wait_ms: u64 = 1500;
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
        cwd: cwd.as_deref(),
        envs: &envs,
    };
    let screen = spawn_and_snapshot(&spec)?;
    print_snapshot(&screen, snap);
    Ok(())
}

/// Inputs for a single PTY run.
struct RunSpec<'a> {
    prog: &'a str,
    args: &'a [String],
    keys: &'a [u8],
    cols: u16,
    rows: u16,
    wait_ms: u64,
    cwd: Option<&'a str>,
    envs: &'a [(String, String)],
}

#[derive(Clone, Copy)]
enum SnapFormat {
    Text,
    Numbered,
    Json,
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

/// Minimal shell-like splitter: splits on whitespace, honours single and
/// double quotes. Good enough for `--bin "recursive tui"` or
/// `--bin "cargo run -q --bin recursive-tui"`.
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse the `--keys` mini-grammar into the bytes actually sent to the PTY.
pub fn parse_keys(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            match n {
                b'r' => out.push(0x0d),
                b'n' => out.push(0x0a),
                b't' => out.push(0x09),
                b'e' => out.push(0x1b),
                b'\\' => out.push(b'\\'),
                b'x' if i + 3 < bytes.len() + 1 => {
                    // \xNN
                    if i + 3 <= bytes.len() {
                        let hex = &s[i + 2..i + 4];
                        if let Ok(v) = u8::from_str_radix(hex, 16) {
                            out.push(v);
                            i += 4;
                            continue;
                        }
                    }
                    out.push(b'\\');
                    out.push(n);
                }
                _ => {
                    out.push(b'\\');
                    out.push(n);
                }
            }
            i += 2;
        } else if b == b'^' && i + 1 < bytes.len() {
            // Ctrl+x = code(control(x)) where control(lowercase letter) = letter - 'a' + 1
            let n = bytes[i + 1];
            let ctrl = match n {
                b'a'..=b'z' => Some(n - b'a' + 1),
                b'A'..=b'Z' => Some(n - b'A' + 1),
                b'[' => Some(0x1b),  // ^[ = ESC
                b']' => Some(0x1d),  // ^] = GS
                b'\\' => Some(0x1c), // ^\ = FS
                b' ' => Some(0),     // ^Space = NUL
                _ => None,
            };
            if let Some(v) = ctrl {
                out.push(v);
                i += 2;
            } else {
                out.push(b'^');
                i += 1;
            }
        } else {
            // UTF-8: push the whole char's bytes.
            let ch = s[i..].chars().next().expect("non-empty char");
            let start = i;
            i += ch.len_utf8();
            out.extend_from_slice(&bytes[start..i]);
        }
    }
    out
}

/// Snapshot of the PTY screen after the run.
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<String>,
}

fn spawn_and_snapshot(spec: &RunSpec) -> Result<Screen> {
    let RunSpec {
        prog,
        args,
        keys,
        cols,
        rows,
        wait_ms,
        cwd,
        envs,
    } = spec;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: *rows,
        cols: *cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(prog);
    for a in *args {
        cmd.arg(a);
    }
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    for (k, v) in *envs {
        cmd.env(k, v);
    }

    let mut child = pair.slave.spawn_command(cmd)?;
    // Drop the slave end after spawning so the master reader receives EOF
    // once the child exits (otherwise the open slave write-end keeps the
    // PTY alive past child exit).
    drop(pair.slave);
    let mut writer = pair.master.take_writer()?;

    // Type the key script with a tiny gap between bytes so the TUI's
    // event loop can keep up (crossterm decodes escape sequences byte by
    // byte; flooding the writer can blur multi-byte keys).
    if !keys.is_empty() {
        for &b in *keys {
            writer.write_all(&[b])?;
            writer.flush()?;
            thread::sleep(Duration::from_millis(2));
        }
    }

    // Reader thread: drain PTY output into a vt100 state model until the
    // child exits or the main thread tears down.
    let parser = Arc::new(Mutex::new(VtParser::new(*rows, *cols, 0)));
    let stop = Arc::new(AtomicBool::new(false));
    let mut reader = pair.master.try_clone_reader()?;
    let (parser_r, stop_r) = (parser.clone(), stop.clone());
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            if stop_r.load(Ordering::Relaxed) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut p) = parser_r.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Give the TUI time to boot, render, and react to the keys.
    thread::sleep(Duration::from_millis(*wait_ms));

    // Snapshot the screen state now, before teardown.
    let screen = {
        let p = parser.lock().map_err(|e| anyhow!("parser lock: {e}"))?;
        let scr = p.screen();
        let (r, c) = scr.size();
        let lines: Vec<String> = (0..r)
            .map(|row| {
                let mut line = String::new();
                for col in 0..c {
                    if let Some(cell) = scr.cell(row, col) {
                        line.push_str(&cell.contents());
                    }
                }
                line.trim_end().to_string()
            })
            .collect();
        Screen {
            cols: c,
            rows: r,
            lines,
        }
    };

    // Teardown: stop the reader, kill the child, reap it. The master
    // (still owned by `pair`) closes when `pair` drops at function return,
    // and the killed child makes the reader hit EOF so the thread exits.
    stop.store(true, Ordering::Relaxed);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    Ok(screen)
}

fn print_snapshot(screen: &Screen, fmt: SnapFormat) {
    match fmt {
        SnapFormat::Text => {
            let mut lines = screen.lines.clone();
            while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                lines.pop();
            }
            println!("{}", lines.join("\n"));
        }
        SnapFormat::Numbered => {
            let width = screen.rows.to_string().len();
            for (i, line) in screen.lines.iter().enumerate() {
                println!("{:>width$}| {line}", i, width = width);
            }
        }
        SnapFormat::Json => {
            let mut lines = screen.lines.clone();
            while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                lines.pop();
            }
            let val = serde_json::json!({
                "width": screen.cols,
                "height": screen.rows,
                "lines": lines,
            });
            println!("{}", serde_json::to_string(&val).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keys_literals_are_utf8_safe() {
        let b = parse_keys("hi→");
        // → is U+2192, 3 bytes in UTF-8; the literal branch must preserve it.
        assert_eq!(b, vec![b'h', b'i', 0xe2, 0x86, 0x92]);
    }

    #[test]
    fn parse_keys_escape_sequences() {
        assert_eq!(parse_keys("a\\rb"), vec![b'a', 0x0d, b'b']);
        assert_eq!(parse_keys("\\n\\t\\e"), vec![0x0a, 0x09, 0x1b]);
        assert_eq!(parse_keys("\\x1b"), vec![0x1b]);
        assert_eq!(parse_keys("\\\\x"), vec![b'\\', b'x']);
    }

    #[test]
    fn parse_keys_ctrl_sequences() {
        assert_eq!(parse_keys("^c"), vec![0x03]);
        assert_eq!(parse_keys("^a"), vec![0x01]);
        assert_eq!(parse_keys("^["), vec![0x1b]); // ^[ = ESC
        assert_eq!(parse_keys("x^m"), vec![b'x', 0x0d]); // ^M = 0x0d (Ctrl+M = CR)
    }

    #[test]
    fn shell_split_handles_quotes_and_whitespace() {
        assert_eq!(shell_split("recursive tui"), vec!["recursive", "tui"]);
        assert_eq!(shell_split("'a b' c"), vec!["a b", "c"]);
        assert_eq!(shell_split("\"cargo run\" -q"), vec!["cargo run", "-q"]);
        assert_eq!(shell_split("  spaced   out "), vec!["spaced", "out"]);
    }

    /// Real PTY smoke test: spawn `echo hello` under a PTY and confirm the
    /// snapshot captures the echoed output. Proves the portable-pty + vt100
    /// pipeline actually reflects what the child wrote.
    #[test]
    fn spawn_and_snapshot_captures_child_output() {
        // `printf` writes without waiting on a tty; echo works on all
        // platforms. Use a short wait and a tiny terminal.
        let spec = RunSpec {
            prog: "echo",
            args: &["hello-pty".to_string()],
            keys: &[],
            cols: 40,
            rows: 5,
            wait_ms: 500,
            cwd: None,
            envs: &[],
        };
        let screen = spawn_and_snapshot(&spec).expect("spawn + snapshot should succeed");
        let text = screen.lines.join("\n");
        assert!(
            text.contains("hello-pty"),
            "snapshot should contain the child's output, got:\n{text}"
        );
    }
}

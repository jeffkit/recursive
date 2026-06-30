//! Engine for `tui-pty`: run a TUI binary under a PTY and snapshot its screen.
//!
//! This is the integration-layer "eyes" for AI-driven TUI testing (stage 4/5).
//! Where the in-process `recursive-tui` harness covers logic + rendering, this
//! crate runs the **real** executable in a pseudo-terminal, parses its ANSI
//! output with a real terminal state model (`vt100`), and exposes the
//! resulting screen — exactly what a user's terminal would show. This is the
//! layer `cargo-mutants` flagged as untested in `recursive-tui`'s `lib.rs`
//! (raw mode, alternate screen, mouse): it only exists behind a real PTY.
//!
//! The crate is a library so integration tests (and other crates' test
//! suites) can call [`spawn_and_snapshot`] directly instead of shelling out
//! to the `tui-pty` binary — which matters because cross-crate binary
//! resolution under `cargo test` is fragile, and a subprocess `cargo run`
//! from inside a test can deadlock on the target-dir build lock. The thin
//! CLI in `src/main.rs` is a convenience wrapper over this engine.
//!
//! # Stability poll
//!
//! [`spawn_and_snapshot`] does NOT sleep a fixed `--wait-ms` and hope the TUI
//! finished. The reader thread tracks when the rendered screen last changed;
//! the main thread snapshots as soon as the screen has been stable for
//! `stable_ms`, capped at `wait_ms` of wall clock. This keeps the harness
//! deterministic on fast machines and non-flaky on slow CI.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use vt100::Parser as VtParser;

/// Inputs for a single PTY run. All fields are owned by the caller; the
/// spec itself borrows for the duration of [`spawn_and_snapshot`].
pub struct RunSpec<'a> {
    pub prog: &'a str,
    pub args: &'a [String],
    pub keys: &'a [u8],
    pub cols: u16,
    pub rows: u16,
    /// Wall-clock cap. The run never waits longer than this.
    pub wait_ms: u64,
    /// Snapshot as soon as the screen is unchanged for this many ms.
    pub stable_ms: u64,
    pub cwd: Option<&'a str>,
    pub envs: &'a [(String, String)],
}

#[derive(Clone, Copy)]
pub enum SnapFormat {
    Text,
    Numbered,
    Json,
}

/// Snapshot of the PTY screen after the run.
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<String>,
}

/// Minimal shell-like splitter: splits on whitespace, honours single and
/// double quotes. Good enough for `--bin "recursive tui"` or
/// `--bin "cargo run -q --bin recursive-tui"`.
pub fn shell_split(s: &str) -> Vec<String> {
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
///
/// `\r` (CR), `\n` (LF), `\t` (TAB), `\e` or `\x1b` (ESC), `^x` (Ctrl+x).
/// Anything else is typed literally (UTF-8 safe).
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
                b'x' if i + 4 <= bytes.len() => {
                    // \xNN — exactly two hex digits follow. The guard
                    // ensures s[i+2..i+4] is in-bounds so the slice and
                    // from_str_radix can't panic; on non-hex input we
                    // emit the backslash literally and let the outer
                    // i += 2 advance past `\\x`, leaving the digits for
                    // the literal branch.
                    let hex = &s[i + 2..i + 4];
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        out.push(v);
                        i += 4;
                        continue;
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

/// Run the TUI under a PTY, type `keys`, wait for the screen to settle,
/// and return the snapshot. See the crate docs for the stability poll.
pub fn spawn_and_snapshot(spec: &RunSpec) -> Result<Screen> {
    let RunSpec {
        prog,
        args,
        keys,
        cols,
        rows,
        wait_ms,
        stable_ms,
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
    // child exits or the main thread tears down. On each chunk it also
    // updates a shared "last screen change" timestamp so the main thread
    // can snapshot the moment the screen goes quiet — instead of sleeping
    // a fixed --wait-ms and hoping the TUI finished.
    let parser = Arc::new(Mutex::new(VtParser::new(*rows, *cols, 0)));
    let stop = Arc::new(AtomicBool::new(false));
    // last_change is set to the run start; reader updates it whenever the
    // rendered screen text actually differs from the previous chunk.
    // got_output records whether the child has written ANY screen-changing
    // output yet — the poll must not declare "stable" before the first
    // render, or a slow-booting TUI gets snapshotted as a blank screen.
    let last_change = Arc::new(Mutex::new(Instant::now()));
    let got_output = Arc::new(AtomicBool::new(false));
    let mut reader = pair.master.try_clone_reader()?;
    let (parser_r, stop_r, last_change_r, got_output_r) = (
        parser.clone(),
        stop.clone(),
        last_change.clone(),
        got_output.clone(),
    );
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut prev: Option<String> = None;
        loop {
            if stop_r.load(Ordering::Relaxed) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut p) = parser_r.lock() {
                        p.process(&buf[..n]);
                        // Track screen stability: compute the current text
                        // and bump last_change only when it differs from
                        // the previous chunk. Cheap for TUI-sized screens
                        // (cols×rows, typically ≤ 80×24); lets the main
                        // thread distinguish "still rendering" from "done"
                        // without a wall-clock guess.
                        let cur = screen_text(&p);
                        if prev.as_deref() != Some(cur.as_str()) {
                            prev = Some(cur);
                            got_output_r.store(true, Ordering::Relaxed);
                            if let Ok(mut lc) = last_change_r.lock() {
                                *lc = Instant::now();
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the screen to settle: poll last_change every 25 ms and
    // snapshot once it has been stable for `stable_ms` — but ONLY after the
    // child has produced output, so a slow boot isn't captured as blank.
    // Cap at `wait_ms` of wall clock so a TUI that never renders (or never
    // stops redrawing) still terminates.
    let start = Instant::now();
    let poll = Duration::from_millis(25);
    let stable = Duration::from_millis(*stable_ms);
    let cap = Duration::from_millis(*wait_ms);
    loop {
        thread::sleep(poll);
        let elapsed_since_change = Instant::now().duration_since(
            *last_change
                .lock()
                .map_err(|e| anyhow!("last_change lock: {e}"))?,
        );
        if (got_output.load(Ordering::Relaxed) && elapsed_since_change >= stable)
            || start.elapsed() >= cap
        {
            break;
        }
    }

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

/// Render the parser's current screen as a single newline-joined string,
/// used by the reader thread for stability detection. Mirrors the line
/// extraction in [`spawn_and_snapshot`] but trimmed of trailing blanks.
fn screen_text(parser: &VtParser) -> String {
    let scr = parser.screen();
    let (r, c) = scr.size();
    let mut out: Vec<String> = (0..r)
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
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// Print a snapshot in the requested format. Used by the `tui-pty` CLI.
pub fn print_snapshot(screen: &Screen, fmt: SnapFormat) {
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
        // echo works on all platforms. stable_ms is small so the snapshot
        // fires as soon as echo's output lands rather than waiting the
        // full 500 ms cap.
        let spec = RunSpec {
            prog: "echo",
            args: &["hello-pty".to_string()],
            keys: &[],
            cols: 40,
            rows: 5,
            wait_ms: 500,
            stable_ms: 80,
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

    /// The stability poll must not block for the full --wait-ms when the
    /// child exits immediately after writing. echo writes one line and
    /// quits; with a 1500 ms cap and a 60 ms stable threshold the run
    /// should finish well under one second. This guards against regressions
    /// where the poll loop accidentally waits the full cap.
    #[test]
    fn stability_poll_returns_early_when_child_exits() {
        let start = Instant::now();
        let spec = RunSpec {
            prog: "echo",
            args: &["fast-pty".to_string()],
            keys: &[],
            cols: 40,
            rows: 5,
            wait_ms: 1500,
            stable_ms: 60,
            cwd: None,
            envs: &[],
        };
        let _ = spawn_and_snapshot(&spec).expect("spawn + snapshot should succeed");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1000),
            "stability poll should return early (got {:?})",
            elapsed
        );
    }
}

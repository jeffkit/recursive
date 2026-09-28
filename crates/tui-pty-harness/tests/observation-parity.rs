//! Issue #45 observation-parity tests: the PTY layer exposes cell attrs,
//! scrollback history, and the raw-stream tee, so the assertions the SOP
//! (`.dev/skills/tui-acceptance.md:42-46`) requires are expressible here.
//! All three tests reuse existing deps only (vt100, portable-pty);
//! `printf` is a POSIX builtin available on macOS/Linux CI (skip on
//! Windows, same convention as the existing PTY smoke tests).

use std::path::PathBuf;

use tui_pty_harness::{spawn_and_snapshot, RunSpec};

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn screen_exposes_cell_attrs_and_row_has_bg_color() {
    // printf emits SGR 41 (red bg) around the marker text, then resets.
    // The integration-layer Screen must surface vt100's parsed bgcolor so
    // the same assertion as recursive-tui/src/harness.rs:113 is possible.
    let spec = RunSpec {
        prog: "printf",
        args: &["\\033[41mREDLINE\\033[0m\\n".to_string()],
        keys: &[],
        cols: 40,
        rows: 5,
        wait_ms: 1000,
        stable_ms: 80,
        cwd: None,
        envs: &[],
        record_raw: None,
    };
    let screen = spawn_and_snapshot(&spec).expect("spawn + snapshot");
    let row = screen
        .find_row("REDLINE")
        .expect("text layer already works");
    // vt100 parses SGR 41 as Color::Idx(1) — same colour ratatui emits for
    // Color::Red over ANSI. Semantics must match Harness::row_has_bg_color
    // (recursive-tui/src/harness.rs:113): "any cell on row y carries bg".
    assert!(
        screen.row_has_bg_color(row, tui_pty_harness::Color::Idx(1)),
        "row {row} should carry red background"
    );
    let attrs = screen.cell_attrs(row, 0).expect("cell in range");
    let _ = attrs; // (contents, fg, bg, bold, underline) per issue proposal
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn scrolled_off_content_is_assertable() {
    // 30 lines on a 10-row screen: lines 0..20 scroll off. With
    // VtParser::new(rows, cols, 0) at lib.rs:226 they are gone forever.
    let spec = RunSpec {
        prog: "sh",
        args: &[
            "-c".to_string(),
            "i=1; while [ $i -le 30 ]; do echo line-$i; i=$((i+1)); done".to_string(),
        ],
        keys: &[],
        cols: 40,
        rows: 10,
        wait_ms: 2000,
        stable_ms: 100,
        cwd: None,
        envs: &[],
        record_raw: None,
    };
    let screen = spawn_and_snapshot(&spec).expect("spawn + snapshot");
    let history = screen.history_lines();
    assert!(
        history.iter().any(|l| l.contains("line-1")),
        "line-1 scrolled off the visible screen but must remain assertable in scrollback; \
         history: {history:?}"
    );
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn record_raw_tees_original_pty_bytes_with_sgr() {
    let raw_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("issue45-raw-pty.log");
    let spec = RunSpec {
        prog: "printf",
        args: &["\\033[41mX\\033[0m".to_string()],
        keys: &[],
        cols: 20,
        rows: 5,
        wait_ms: 1000,
        stable_ms: 80,
        cwd: None,
        envs: &[],
        record_raw: Some(&raw_path),
    };
    let _ = spawn_and_snapshot(&spec).expect("spawn + snapshot");
    let raw = std::fs::read(&raw_path).expect("record_raw must tee the raw PTY stream");
    assert!(
        raw.windows(5).any(|w| w == b"\x1b[41m"),
        "raw stream must contain the SGR sequence the child emitted, got {raw:?}"
    );
}

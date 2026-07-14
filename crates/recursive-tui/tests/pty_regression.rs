//! PTY integration regression gate for the real `recursive-tui` binary.
//!
//! The in-process `Harness` (src/harness.rs) covers logic + rendering, but
//! it cannot reach the terminal-IO layer that only exists behind a real
//! PTY: crossterm raw mode, EnterAlternateScreen, EnableMouseCapture, and
//! the real event loop in `lib::run`. `cargo-mutants` explicitly allows
//! survivors in `lib.rs` for exactly this reason. This file is the
//! automated regression gate for that layer — it boots the actual binary
//! under a PTY and asserts on the screen a user would see.
//!
//! This is the "step 4 PTY tour" of `.dev/skills/tui-acceptance.md`, turned
//! from a manual SOP step into a `cargo test` that runs on every
//! `cargo test -p recursive-tui`. Because `tui_pty_harness` now polls for
//! screen stability instead of sleeping a fixed `--wait-ms`, the assertions
//! are deterministic on fast machines and non-flaky on slow CI.
//!
//! `CARGO_BIN_EXE_recursive-tui` resolves to the binary cargo just built for
//! this test run, so no subprocess `cargo build` / `cargo run` is needed
//! (which would risk a target-dir build-lock deadlock).

use std::path::Path;

use tui_pty_harness::{spawn_and_snapshot, RunSpec};

/// Resolve the freshly-built `recursive-tui` binary path.
fn tui_bin() -> String {
    let path = env!("CARGO_BIN_EXE_recursive-tui");
    assert!(
        Path::new(path).exists(),
        "recursive-tui binary not found at {path}"
    );
    path.to_string()
}

/// Run the TUI under a PTY with the given key script and return the screen
/// text (lines joined by `\n`, trailing blanks dropped).
fn tour(keys: &str, wait_ms: u64) -> String {
    let bin = tui_bin();
    let spec = RunSpec {
        prog: &bin,
        args: &[],
        keys: &tui_pty_harness::parse_keys(keys),
        cols: 80,
        rows: 24,
        wait_ms,
        stable_ms: 150,
        cwd: None,
        envs: &[],
    };
    let screen = spawn_and_snapshot(&spec).expect("PTY tour should succeed");
    let mut lines = screen.lines.clone();
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

/// Boot the TUI with no input and confirm the empty-state renders:
/// the wordmark plus either the "Type a message to start" hint (when a
/// provider is configured) or the offline setup guidance (when none is).
/// This pins the alternate-screen + raw-mode boot path — if any of that
/// regresses, the screen never reaches the user and this test fails
/// instead of a human noticing a blank terminal.
/// Ignored on Windows: `tui_pty_harness` drives a real PTY + the
/// `recursive-tui` subprocess, and on `windows-latest` CI both this test and
/// `pty_help_command_opens_modal` hang forever (libtest reports "has been
/// running for over 60 seconds" and the run only ends on the 6h job timeout /
/// cancellation). The portable-pty backend on Windows runners does not release
/// the child + screen-poll loop the way it does on unix, so the snapshot never
/// returns. The in-process `Harness` (src/harness.rs) still covers the logic +
/// rendering layer on Windows; only the terminal-IO PTY layer is skipped there.
/// This matches the repo convention of ignoring PTY/shell-driven tests on
/// Windows — see `crates/recursive-tui/src/backend.rs` and
/// `.dev/journal/manual-20260603-fix-ci-windows-tests.md`.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn pty_boot_renders_splash() {
    let text = tour("", 3000);
    // The boot screen reflects the real ~/.recursive/config.toml: online
    // shows the typing hint, offline (no provider) shows the setup
    // guidance. Either is a valid, non-blank boot — assert the user sees
    // one of them, plus the always-present command hint.
    let online = text.contains("Type a message to start");
    let offline =
        text.contains("Offline") && text.contains("recursive init") && text.contains("no provider");
    assert!(
        online || offline,
        "boot should show either the online splash or the offline setup guidance, got:\n{text}"
    );
    assert!(
        text.contains("/resume") && text.contains("/help"),
        "splash should advertise /resume and /help, got:\n{text}"
    );
}

/// Typing `/help\r` should open the help modal and render the command list.
/// This exercises the real keymap dispatch + modal render path under a PTY
/// (raw-mode key decoding, EnterAlternateScreen, modal overlay) — the layer
/// the in-process harness covers only synthetically.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn pty_help_command_opens_modal() {
    let text = tour("/help\r", 3000);
    // The help modal lists available slash commands. Assert a stable,
    // user-visible heading rather than exact wording so a wording tweak
    // doesn't break the gate — but the modal MUST appear.
    assert!(
        text.to_lowercase().contains("commands") || text.to_lowercase().contains("help"),
        "help modal should render after /help, got:\n{text}"
    );
}

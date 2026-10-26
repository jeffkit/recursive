//! Process-level pins for the `recursive-eval` bin (issue #245 batch 1).
//!
//! `main`'s exit mapping and `list`'s stdout catalogue are only observable
//! from outside the process, so these spawn the compiled binary via the
//! cargo-set `CARGO_BIN_EXE_recursive-eval` env var (same pattern as the
//! `mock_mcp_server` pins in `acp_tool_surface.rs`). Both tests need no
//! config, no API key, and no env.

use std::process::Command;

fn eval_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_recursive-eval"))
}

/// `main` must map a failed `dispatch` to exit code 2 (replacing `main` with
/// `Default::default()` would exit 0 silently).
#[test]
fn eval_binary_exits_2_on_unknown_command() {
    let out = eval_bin()
        .arg("definitely-not-a-command")
        .output()
        .expect("spawn recursive-eval");
    assert_eq!(out.status.code(), Some(2), "status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown command"), "stderr: {stderr}");
}

/// `list` prints the task and preset catalogue (a `list -> ()` mutant prints
/// nothing and still exits 0).
#[test]
fn eval_binary_list_prints_task_and_preset_catalogue() {
    let out = eval_bin()
        .arg("list")
        .output()
        .expect("spawn recursive-eval");
    assert!(out.status.success(), "status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tasks ("), "stdout: {stdout}");
    assert!(stdout.contains("presets:"), "stdout: {stdout}");
    assert!(
        stdout.contains("single-append-row"),
        "task catalogue line missing: {stdout}"
    );
}

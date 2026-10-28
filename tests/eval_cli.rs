//! Process-level pins for the `recursive-eval` bin (issue #245 batch 1).
//!
//! `main`'s exit mapping and `list`'s stdout catalogue are only observable
//! from outside the process, so these spawn the compiled binary via the
//! cargo-set `CARGO_BIN_EXE_recursive-eval` env var (same pattern as the
//! `mock_mcp_server` pins in `acp_tool_surface.rs`). Both tests need no
//! config, no API key, and no env.

use std::path::PathBuf;
use std::process::Command;

fn eval_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_recursive-eval"))
}

/// One replay `Sample` serialized through the real `eval::to_jsonl`, so a
/// field rename breaks this fixture at compile time instead of rotting a
/// hand-written JSON literal.
fn sample_jsonl() -> String {
    use recursive::eval::Sample;
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
    recursive::eval::to_jsonl(&[sample]).expect("to_jsonl")
}

/// Fresh per-test scratch directory; the child's cwd is pointed here so any
/// relative path a mutant invents (`--out` → the flag itself) lands in temp
/// space instead of the package root.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recursive-eval-cli-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
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

/// `run` in replay mode must actually drive the suite and write both the
/// JSONL and the markdown destinations (a `cmd_run -> Ok(())` mutant exits 0
/// having written nothing). One task under one preset keeps it to a single
/// scripted agent run; every destination is relative to the scratch cwd.
#[test]
fn eval_binary_run_replay_writes_jsonl_and_report() {
    let root = scratch("run");
    let out = eval_bin()
        .args([
            "run",
            "--presets",
            "minimal",
            "--tasks",
            "single-append-row",
            "--out",
            "out.jsonl",
            "--report",
            "rep.md",
            "--workdir",
            "work",
        ])
        .current_dir(&root)
        .output()
        .expect("spawn recursive-eval run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "status: {:?} stderr: {stderr}",
        out.status
    );
    let jsonl = std::fs::read_to_string(root.join("out.jsonl")).expect("out.jsonl written");
    assert_eq!(jsonl.lines().count(), 1, "one sample expected: {jsonl}");
    assert!(
        jsonl.contains("\"task_id\":\"single-append-row\""),
        "{jsonl}"
    );
    assert!(jsonl.contains("\"preset\":\"minimal\""), "{jsonl}");
    assert!(jsonl.contains("\"success\":true"), "{jsonl}");
    let report = std::fs::read_to_string(root.join("rep.md")).expect("rep.md written");
    assert!(report.contains("# Preset benchmark"), "{report}");
    assert!(report.contains("| minimal |"), "{report}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("wrote 1 samples to"), "stdout: {stdout}");
    assert!(
        stderr.contains("pass"),
        "per-sample progress line: {stderr}"
    );
}

/// `report --out <file>` must write the rendered markdown to the *next*
/// argument. The `args.get(i + 1)` → `get(i)` mutant reads the flag itself,
/// so it writes a file literally named `--out` and never creates the named
/// one — only observable from a process whose cwd we control.
#[test]
fn eval_binary_report_out_flag_writes_named_file() {
    let root = scratch("report");
    std::fs::write(root.join("in.jsonl"), sample_jsonl()).expect("seed in.jsonl");
    let out = eval_bin()
        .args(["report", "in.jsonl", "--out", "rep.md"])
        .current_dir(&root)
        .output()
        .expect("spawn recursive-eval report");
    assert!(
        out.status.success(),
        "status: {:?} stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let report = std::fs::read_to_string(root.join("rep.md")).expect("rep.md written");
    assert!(report.contains("# Preset benchmark"), "{report}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("wrote report to rep.md"),
        "stdout: {stdout}"
    );
}

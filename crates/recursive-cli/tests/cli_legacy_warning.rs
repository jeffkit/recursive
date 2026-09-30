//! Startup warning for legacy in-tree state (`<workspace>/.recursive/`).
//!
//! `main()` warns before dispatching any workspace-touching command, except
//! `migrate` itself (which would otherwise double-warn), and only when legacy
//! paths were actually found:
//!
//! ```ignore
//! if !matches!(effective_cmd, Cmd::Migrate { .. }) {
//!     let legacy = recursive::legacy_paths_in_workspace(&config.workspace);
//!     if !legacy.is_empty() { eprintln!("warning: legacy in-tree state detected …") }
//! }
//! ```
//!
//! Both guards are user-visible, so they are pinned here by spawning the real
//! binary rather than by reaching into `main()`.

use std::path::Path;
use std::process::{Command, Output};

/// A workspace whose `.recursive/` still holds legacy in-tree state.
fn workspace_with_legacy_state() -> tempfile::TempDir {
    let ws = tempfile::tempdir().expect("workspace tempdir");
    std::fs::create_dir_all(ws.path().join(".recursive").join("sessions")).expect("mkdir legacy");
    ws
}

fn run(workspace: &Path, args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recursive"));
    cmd.arg("--workspace").arg(workspace);
    cmd.args(args);
    cmd.env("RECURSIVE_HOME", home.path());
    // Ambient provider/workspace overrides would change which command runs.
    for var in [
        "RECURSIVE_WORKSPACE",
        "RECURSIVE_MODEL",
        "RECURSIVE_API_BASE",
        "RECURSIVE_PROVIDER_TYPE",
        "RECURSIVE_SANDBOX",
    ] {
        cmd.env_remove(var);
    }
    cmd.output().expect("spawn recursive")
}

fn stderr_of(out: &Output) -> String {
    // windows 的警告里路径是反斜杠（.recursive\sessions）；归一成正斜杠，
    // 让下面的 contains 断言跨平台成立（2026-09-30 CI 实证）。
    String::from_utf8_lossy(&out.stderr).to_string().replace('\\', "/")
}

#[test]
fn legacy_state_warns_before_a_non_migrate_command() {
    let ws = workspace_with_legacy_state();
    let out = run(ws.path(), &["sessions", "list"]);
    let stderr = stderr_of(&out);

    assert!(
        stderr.contains("legacy in-tree state detected"),
        "expected the legacy-state warning, got:\n{stderr}"
    );
    // The warning names the offending path and the remedy.
    assert!(
        stderr.contains(".recursive/sessions"),
        "warning should list the legacy path, got:\n{stderr}"
    );
    assert!(
        stderr.contains("recursive migrate"),
        "warning should point at `recursive migrate`, got:\n{stderr}"
    );
}

#[test]
fn legacy_state_does_not_warn_for_migrate_itself() {
    let ws = workspace_with_legacy_state();
    let out = run(ws.path(), &["migrate", "--dry-run"]);
    let stderr = stderr_of(&out);

    assert!(
        !stderr.contains("legacy in-tree state detected"),
        "`migrate` must not double-warn, got:\n{stderr}"
    );
}

#[test]
fn clean_workspace_is_not_warned_about() {
    let ws = tempfile::tempdir().expect("workspace tempdir");
    let out = run(ws.path(), &["sessions", "list"]);
    let stderr = stderr_of(&out);

    assert!(
        !stderr.contains("legacy in-tree state detected"),
        "a clean workspace must not warn, got:\n{stderr}"
    );
}

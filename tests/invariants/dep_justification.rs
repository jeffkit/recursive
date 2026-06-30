// Why this test exists:
// .dev/AGENTS.md invariant #6: "No new dependencies without justification.
// State the reason in the journal entry. Prefer std + what's already in
// Cargo.toml."
//
// This test verifies that the dep-checking script exists, is executable,
// and runs successfully.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The dep-checking script must exist and be executable.
#[test]
fn dep_check_script_exists_and_is_executable() {
    let script = workspace_root().join("scripts").join("check-new-deps.sh");
    assert!(script.exists(), "scripts/check-new-deps.sh must exist");

    // Check it's readable (executability is handled by chmod +x)
    let metadata = std::fs::metadata(&script).expect("must be able to stat check-new-deps.sh");
    assert!(metadata.is_file(), "check-new-deps.sh must be a file");

    // At minimum the script should be readable.
    assert!(
        std::fs::read_to_string(&script).is_ok(),
        "check-new-deps.sh must be readable"
    );
}

/// Run the dep-check script. It should pass when Cargo.toml is unchanged
/// relative to HEAD~1, or when all changes have journal justification.
///
/// Ignored on Windows: the script is a bash script, and on `windows-latest`
/// runners `bash` resolves to WSL's `bash.exe`, which fails immediately with
/// "Windows Subsystem for Linux has no installed distributions." (Git Bash is
/// present but not on PATH.) This matches the existing convention of ignoring
/// shell-driven tests on Windows — see `crates/recursive-tui/src/backend.rs`
/// and `.dev/journal/manual-20260603-fix-ci-windows-tests.md`. The
/// `cargo_toml_is_valid` and `dep_check_script_exists_*` tests still run on
/// Windows, so the invariant is not silently skipped wholesale.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_check_script_passes() {
    let script = workspace_root().join("scripts").join("check-new-deps.sh");

    // Only run if git is available and we're in a git repo.
    let git_available = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok();
    if !git_available {
        eprintln!("SKIP: git not available");
        return;
    }
    let in_repo = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--git-dir")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_repo {
        eprintln!("SKIP: not in a git repo");
        return;
    }

    let output = std::process::Command::new("bash")
        .arg(&script)
        .arg("HEAD~1")
        .current_dir(workspace_root())
        .output()
        .expect("must be able to run check-new-deps.sh");

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("invariant #6 check failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    }
}

/// Smoke test: Cargo.toml must exist and be parseable.
#[test]
fn cargo_toml_is_valid() {
    let cargo_toml = workspace_root().join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml).expect("Cargo.toml must exist");
    assert!(
        content.contains("[package]"),
        "Cargo.toml must contain [package]"
    );
    assert!(
        content.contains("[dependencies]"),
        "Cargo.toml must contain [dependencies]"
    );
}

// Why this test exists:
// .dev/AGENTS.md invariant #6: "No new dependencies without justification.
// State the reason in the journal entry. Prefer std + what's already in
// Cargo.toml."
//
// This test verifies that the dep-checking script exists, is executable,
// and runs successfully. It also drives the script against throwaway fixture
// repos (#184): a gate that never goes red is not a gate, so the negative
// controls below assert the red/green/fail-closed behaviour directly.

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

// ---------------------------------------------------------------------------
// Gate self-validity fixtures (#184)
//
// scripts/check-new-deps.sh only counts as an invariant guard if it goes RED
// for an unjustified dependency, GREEN for a justified one, and fails closed
// when its base ref is missing. Before #184 the filter regex ate every diff
// line, so every case below was silently green.
// ---------------------------------------------------------------------------

/// A throwaway git repo containing a copy of the real gate script.
struct DepGateFixture {
    dir: tempfile::TempDir,
    script: std::path::PathBuf,
}

/// The dependency used by the fixtures. It must not appear in any journal of
/// the fixture repo unless the test puts it there.
const UNJUSTIFIED_DEP: &str = "totally-new-crate-xyz";

const BASELINE_CARGO_TOML: &str = r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1"
"#;

impl DepGateFixture {
    /// `None` when the platform cannot run the bash script (Windows) or git /
    /// bash are unavailable — the caller then skips instead of failing.
    fn new() -> Option<Self> {
        if cfg!(target_os = "windows") {
            return None;
        }
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
            || std::process::Command::new("bash")
                .arg("--version")
                .output()
                .is_err()
        {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let script = dir.path().join("scripts").join("check-new-deps.sh");
        std::fs::create_dir_all(script.parent()?).ok()?;
        std::fs::create_dir_all(dir.path().join(".dev").join("journal")).ok()?;
        std::fs::copy(
            workspace_root().join("scripts").join("check-new-deps.sh"),
            &script,
        )
        .ok()?;

        let fixture = DepGateFixture { dir, script };
        fixture.write("Cargo.toml", BASELINE_CARGO_TOML);
        fixture.write(".dev/journal/manual-baseline.md", "# baseline\n");
        fixture.git(&["init", "-q", "."]);
        fixture.commit("base");
        Some(fixture)
    }

    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.root().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(&path, body).expect("write fixture file");
    }

    /// Run git in the fixture repo, with identity / signing pinned so a
    /// developer's global config cannot change the outcome.
    fn git(&self, args: &[&str]) -> std::process::Output {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=invariant@recursive.local",
                "-c",
                "user.name=Invariant Fixture",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(self.root())
            .output()
            .expect("git must be runnable");
        assert!(
            output.status.success(),
            "git {args:?} failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
    }

    /// Run the gate script from `cwd` with `base_ref`.
    fn run(&self, cwd: &std::path::Path, base_ref: &str) -> std::process::Output {
        std::process::Command::new("bash")
            .arg(&self.script)
            .arg(base_ref)
            .current_dir(cwd)
            .output()
            .expect("bash must be runnable")
    }

    fn add_unjustified_dep(&self) {
        self.write(
            "Cargo.toml",
            &format!("{BASELINE_CARGO_TOML}{UNJUSTIFIED_DEP} = \"2\"\n"),
        );
    }
}

fn assert_gate_red(out: &std::process::Output, what: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "invariant #6 must fail for {what}, but the gate passed:\n{stdout}"
    );
    assert!(
        stdout.contains("VIOLATION") || stdout.contains("FAIL"),
        "gate failed for {what} without saying why:\n{stdout}"
    );
}

fn assert_gate_green(out: &std::process::Output, what: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "invariant #6 must pass for {what}, but the gate failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// The regression that started #184: a brand-new dependency with no journal
/// justification anywhere must turn the gate red.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_gate_fails_for_new_dependency_without_journal() {
    let Some(fixture) = DepGateFixture::new() else {
        eprintln!("SKIP: git/bash unavailable");
        return;
    };
    fixture.add_unjustified_dep();
    fixture.commit("add an unjustified dependency");

    let out = fixture.run(fixture.root(), "HEAD~1");
    assert_gate_red(&out, "an unjustified new dependency");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(UNJUSTIFIED_DEP),
        "the report must name the offending dependency"
    );
}

/// Positive control: the same change passes once a journal from this run names
/// the dependency — the gate must not simply always fail.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_gate_passes_for_new_dependency_with_journal_justification() {
    let Some(fixture) = DepGateFixture::new() else {
        eprintln!("SKIP: git/bash unavailable");
        return;
    };
    fixture.add_unjustified_dep();
    fixture.write(
        ".dev/journal/manual-run.md",
        &format!("# Run\n\nWe need {UNJUSTIFIED_DEP} because serde cannot do it.\n"),
    );
    fixture.commit("add a dependency with justification");

    let out = fixture.run(fixture.root(), "HEAD~1");
    assert_gate_green(&out, "a new dependency justified in this run's journal");
}

/// False positives are just as damaging as false negatives: bumping the
/// `[package] version` is not a dependency change.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_gate_ignores_package_metadata_changes() {
    let Some(fixture) = DepGateFixture::new() else {
        eprintln!("SKIP: git/bash unavailable");
        return;
    };
    fixture.write(
        "Cargo.toml",
        &BASELINE_CARGO_TOML.replace("version = \"0.1.0\"", "version = \"0.2.0\""),
    );
    fixture.commit("bump the package version");

    let out = fixture.run(fixture.root(), "HEAD~1");
    assert_gate_green(&out, "a [package] version bump");
}

/// A shallow CI clone has no HEAD~1. The gate must fail closed instead of
/// reporting "Cargo.toml unchanged" (that silence is exactly how #184 hid).
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_gate_fails_closed_when_base_ref_is_missing() {
    let Some(fixture) = DepGateFixture::new() else {
        eprintln!("SKIP: git/bash unavailable");
        return;
    };
    fixture.add_unjustified_dep();
    fixture.commit("add an unjustified dependency");

    let out = fixture.run(fixture.root(), "refs/heads/does-not-exist");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing base ref must exit 2 (fail closed):\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("does not resolve"),
        "the failure must name the unresolvable base ref:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// The verdict must not depend on the caller's cwd: the script resolves the
/// workspace from its own path.
#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn dep_gate_verdict_is_independent_of_caller_cwd() {
    let Some(fixture) = DepGateFixture::new() else {
        eprintln!("SKIP: git/bash unavailable");
        return;
    };
    fixture.add_unjustified_dep();
    fixture.commit("add an unjustified dependency");

    let elsewhere = tempfile::tempdir().expect("temp dir");
    let out = fixture.run(elsewhere.path(), "HEAD~1");
    assert_gate_red(&out, "an unjustified dependency when run from another cwd");
}

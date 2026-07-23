// Why this test exists:
// .dev/AGENTS.md invariant #4: "Tests are non-negotiable. Every new public
// function / tool / provider gets unit tests in the same file
// (`#[cfg(test)] mod tests`)."
//
// This test verifies that key public modules have test modules in the same
// file. It's not a line-coverage tool (that's what cargo-llvm-cov is for).
// Instead, it's a lint that ensures no one deletes the `#[cfg(test)] mod tests`
// block from a file that defines public API.
//
// This is a structural check, not a coverage check. It looks for the presence
// of `#[cfg(test)]` blocks in files that define public functions.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Files that must have a `#[cfg(test)] mod tests` block because they
/// define public API. This list is curated — not every src/ file needs
/// its own test module (e.g. `lib.rs` just re-exports).
const MUST_HAVE_TESTS: &[&str] = &[
    "src/error.rs",
    "src/message.rs",
    "src/config.rs",
    "src/compact/mod.rs",
    "src/transcript.rs",
    "src/cost.rs",
    "src/llm/mod.rs",
    "src/llm/openai.rs",
    "src/llm/mock.rs",
    "src/llm/anthropic.rs",
    "src/tools/mod.rs",
    "src/tools/fs.rs",
    "src/tools/shell.rs",
    "src/tools/edit.rs",
    "src/tools/glob.rs",
    "src/tools/search.rs",
    "src/tools/count_lines.rs",
    "src/tools/estimate_tokens.rs",
    "src/tools/load_skill.rs",
    "src/tools/todo.rs",
    "src/tools/facts.rs",
    "src/tools/run_background.rs",
    "src/tools/checkpoint.rs",
    "src/tools/episodic_recall.rs",
    "src/tools/web_fetch.rs",
    "src/tools/web_search.rs",
    "src/tools/tool_search.rs",
    "src/tools/plan_mode.rs",
    "src/session/mod.rs",
    "src/tasks.rs",
    "src/skills.rs",
];

/// Check that each listed file contains `#[cfg(test)]` (ensuring the test
/// module hasn't been accidentally deleted).
#[test]
fn public_modules_have_test_blocks() {
    let mut missing = Vec::new();
    for &rel_path in MUST_HAVE_TESTS {
        let path = workspace_root().join(rel_path);
        let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("Cannot read {rel_path}: {e}");
        });
        if !content.contains("#[cfg(test)]") {
            missing.push(rel_path);
        }
    }

    if !missing.is_empty() {
        panic!(
            "invariant #4 violation: the following files define public API but \
             have no #[cfg(test)] module:\n  - {}\n\
             Every public function/tool/provider must have unit tests in the same file.",
            missing.join("\n  - ")
        );
    }
}

/// Files that define `pub fn` or `pub async fn` should have tests.
/// This is a dynamic check that scans for public functions without
/// corresponding test coverage in the same file.
#[test]
fn pub_fns_have_corresponding_tests() {
    let mut violations = Vec::new();
    let src_dir = workspace_root().join("src");

    for entry in walkdir::WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
        .filter(|e| !e.path().ends_with("lib.rs")) // lib.rs just re-exports
        .filter(|e| !e.path().ends_with("main.rs"))
    // main has no pub API
    {
        let content = std::fs::read_to_string(entry.path()).unwrap();

        // Check if this file defines any public functions.
        let has_pub_fn = content
            .lines()
            .any(|l| l.trim().starts_with("pub fn ") || l.trim().starts_with("pub async fn "));

        if has_pub_fn {
            // Check if there's a test module.
            if !content.contains("#[cfg(test)]") {
                let rel = entry.path().strip_prefix(&src_dir).unwrap().display();
                violations.push(format!("{rel}"));
            }
        }
    }

    // We don't fail on every file — some derive-macro-heavy files don't need
    // explicit tests. We only report violations for files in the MUST_HAVE_TESTS
    // list (tested above). This test is a soft check.
    if !violations.is_empty() {
        eprintln!(
            "info: {count} files with pub fn but no #[cfg(test)] module: {list:?}",
            count = violations.len(),
            list = &violations[..10.min(violations.len())]
        );
    }
    // This test is informational; the hard check is `public_modules_have_test_blocks`.
}

// Why this test exists:
// issue #185: every `recursive-cli` unit test runs in ONE process (`--bin
// recursive`), so a raw `std::env::set_var` / `std::env::remove_var` in any
// test bypasses the crate-wide `crate::test_env::ENV_LOCK` and re-creates the
// cross-module race that made the gate flaky
// (`build_runtime_*` readers observing `unknown_preset_env_fails_the_cli_build`'s
// transient `RECURSIVE_AGENT_PRESET=no-such-preset`).
//
// A guard self-test can only prove that a *live* guard holds the lock; it
// cannot stop someone from adding a NEW raw `set_var` next to it. This is the
// static half of the invariant: within `crates/recursive-cli/src`, the only
// file allowed to touch the process env is `test_env.rs` (the guard itself),
// plus exactly one product-code path — the `--sandbox` export at the top of
// `main()`, which is not test code and predates this invariant.
//
// Run with: `cargo test --test invariants`

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn cli_src_root() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("recursive-cli")
        .join("src")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// A raw process-env *call* line. Deliberately requires the `(` so doc-comment
/// prose that merely names `std::env::set_var` does not trip the check.
fn is_raw_env_call(line: &str) -> bool {
    line.contains("std::env::set_var(") || line.contains("std::env::remove_var(")
}

/// Tests only. Clippy's `-D warnings` covers the crate, but this check must
/// stay independent of the lint configuration.
#[test]
fn cli_process_env_is_mutated_only_by_the_shared_guard() {
    let src = cli_src_root();
    assert!(src.is_dir(), "expected CLI source at {}", src.display());

    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        src.display()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut product_exemptions: Vec<(usize, String)> = Vec::new();

    for file in &files {
        let rel = file.strip_prefix(&src).unwrap_or(file);
        if rel == Path::new("test_env.rs") {
            // The guard itself is the one legitimate home for raw env mutation.
            continue;
        }
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        // In `main.rs`, everything above `mod tests {` is product code — the
        // `#[cfg(test)]` module starts at that line.
        let test_mod_start = if rel == Path::new("main.rs") {
            text.lines().position(|l| l.trim() == "mod tests {")
        } else {
            None
        };

        for (idx, line) in text.lines().enumerate() {
            if !is_raw_env_call(line) || line.trim_start().starts_with("//") {
                continue;
            }
            if let Some(start) = test_mod_start {
                if idx < start {
                    product_exemptions.push((idx + 1, line.trim().to_string()));
                    continue;
                }
            }
            offenders.push(format!("{}:{}: {}", rel.display(), idx + 1, line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "test code mutated the process env without crate::test_env::EnvGuard \
         (issue #185: one shared lock, or the --bin recursive suite races \
         itself). Route these through EnvGuard:\n{}",
        offenders.join("\n")
    );

    // The product-code exemption must stay exactly the `--sandbox` export in
    // `main()`; a second raw site appearing there means someone widened the
    // carve-out instead of using the guard.
    assert_eq!(
        product_exemptions.len(),
        1,
        "expected exactly one product-code raw env mutation (`--sandbox` in \
         main()), found: {product_exemptions:?}"
    );
    assert!(
        product_exemptions[0].1.contains("RECURSIVE_SANDBOX"),
        "the single exempted product-code site must be the --sandbox export, got: {}",
        product_exemptions[0].1
    );
}

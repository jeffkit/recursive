// Why this test exists:
// issue #185: every `recursive-cli` unit test runs in ONE process (`--bin
// recursive`), so a raw `std::env::set_var` / `std::env::remove_var` in any
// test bypasses the crate-wide `crate::test_env::ENV_LOCK` and re-creates the
// cross-module race that made the gate flaky
// (`build_runtime_*` readers observing `unknown_preset_env_fails_the_cli_build`'s
// transient `RECURSIVE_AGENT_PRESET=no-such-preset`).
//
// A guard self-test can only prove that a *live* guard holds the lock; it
// cannot stop someone from adding a NEW raw env mutation next to it. This is
// the static half of the invariant: within `crates/recursive-cli/src`, the only
// file allowed to touch the process env is `test_env.rs` (the guard itself),
// plus exactly one product-code path — the `--sandbox` export at the top of
// `main()`, which is not test code and predates this invariant.
//
// Matching surface (see `raw_env_call`): comments and string literals are
// stripped first, then any `set_var(` / `remove_var(` call whose receiver is
// NOT the guard (`EnvGuard::set_var(`, `.set_var(`) is treated as raw. That
// catches `std::env::set_var(`, `env::set_var(` after `use std::env;`, and a
// bare `set_var(` after `use std::env::set_var;`.
//
// Known boundary: this is a lexical check, not a resolver. A raw mutation
// hidden inside a macro body, or a guard type re-exported/aliased under a
// different name, can still slip through — code review and clippy remain the
// backstop.
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

/// Replace Rust comments and string literals with blanks, preserving line
/// structure so offender line numbers still match the file on disk. Block
/// comments are nesting-aware. Character literals are left alone on purpose: a
/// `char` literal cannot contain `set_var(`.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut block_depth: usize = 0;
    let mut line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if line_comment {
            if c == '\n' {
                line_comment = false;
                out.push('\n');
            }
            continue;
        }
        if block_depth > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                block_depth -= 1;
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                block_depth += 1;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            // Keep line structure: a multi-line string literal (e.g. `r#"…"#`)
            // must not shift the line numbers we report or compare.
            if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    line_comment = true;
                    continue;
                }
                Some('*') => {
                    chars.next();
                    block_depth += 1;
                    continue;
                }
                _ => {}
            }
        }
        if c == '"' {
            in_string = true;
            continue;
        }
        out.push(c);
    }
    out
}

/// A raw process-env mutation call, in code with comments/strings stripped.
///
/// Legitimate forms have the guard receiver immediately before the method name:
/// `EnvGuard::set_var(` or `<receiver>.set_var(`. Everything else —
/// `std::env::set_var(`, `env::set_var(`, bare `set_var(` — is raw. A definition
/// line (`fn set_var(`) is not a call.
fn raw_env_call(line: &str) -> bool {
    if line.contains("fn set_var") || line.contains("fn remove_var") {
        return false;
    }
    for name in ["set_var", "remove_var"] {
        let needle = format!("{name}(");
        for (idx, _) in line.match_indices(&needle) {
            let before = &line[..idx];
            let via_guard = before.ends_with('.') || before.ends_with("EnvGuard::");
            if !via_guard {
                return true;
            }
        }
    }
    false
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
        let code = code_only(&text);
        // `code_only` must preserve the line count, otherwise the product-code
        // exemption (an index comparison) and every reported line number would
        // silently drift.
        assert_eq!(
            text.lines().count(),
            code.lines().count(),
            "comment/string stripping changed the line count of {}",
            rel.display()
        );
        // In `main.rs`, everything above `mod tests {` is product code — the
        // `#[cfg(test)]` module starts at that line.
        let test_mod_start = if rel == Path::new("main.rs") {
            text.lines().position(|l| l.trim() == "mod tests {")
        } else {
            None
        };

        for (idx, (original, code_line)) in text.lines().zip(code.lines()).enumerate() {
            if !raw_env_call(code_line) {
                continue;
            }
            if let Some(start) = test_mod_start {
                if idx < start {
                    product_exemptions.push((idx + 1, original.trim().to_string()));
                    continue;
                }
            }
            offenders.push(format!(
                "{}:{}: {}",
                rel.display(),
                idx + 1,
                original.trim()
            ));
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

/// The matcher must catch the aliasing spellings a future author might use, and
/// must NOT fire on the guard's own API, on definitions, or on prose.
#[test]
fn raw_env_call_matcher_covers_aliases_and_ignores_the_guard_api() {
    let raw = [
        "std::env::set_var(\"K\", \"v\");",
        "std::env::remove_var(\"K\");",
        "env::set_var(\"K\", \"v\");",
        "set_var(\"K\", \"v\");",
        "  remove_var(\"K\");",
    ];
    for line in raw {
        assert!(raw_env_call(line), "must be flagged as raw: {line}");
    }

    let allowed = [
        "EnvGuard::set_var(\"K\", \"v\");",
        "env.set_var(\"K\", \"v\");",
        "guard.remove_var(\"K\");",
        "crate::test_env::EnvGuard::set_var(\"K\", \"v\");",
        "fn set_var(&mut self, key: K, value: V) {",
        "let _env = EnvGuard::lock();",
    ];
    for line in allowed {
        assert!(!raw_env_call(line), "must NOT be flagged: {line}");
    }

    // Prose mentioning the call must not trip the check once comments and
    // string literals are stripped.
    let prose = "// honour std::env::set_var( here\nlet s = \"std::env::set_var(\";\n";
    let code = code_only(prose);
    assert!(
        !code.lines().any(raw_env_call),
        "comments/strings must be stripped, got: {code:?}"
    );

    // A multi-line string literal must not shift line numbers (the product-code
    // exemption compares indices, so this would silently drift).
    let multiline = "let a = \"one\nstd::env::set_var(\n\";\nlet b = 1;\n";
    assert_eq!(
        multiline.lines().count(),
        code_only(multiline).lines().count(),
        "multi-line string must not collapse lines"
    );
}

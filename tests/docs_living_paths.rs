// Issue #59: living docs must not reference paths that no longer exist.
// Scope: docs/ (minus docs/review and docs/exec-plans, which are dated
// snapshots), README.md, .dev/AGENTS.md, root AGENTS.md / CLAUDE.md.
// .dev/journal/** is explicitly exempt — frozen history, drift is correct.

use std::fs;
use std::path::Path;

const SCAN_FILES: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".dev/AGENTS.md",
    "docs/tui-acceptance-checklist.md",
    "docs/tui-fake-cc-gap.md",
    "docs/llm-gateway-compat.md",
    "docs/INTERNALS.md",
    ".dev/OPERATIONS.md",
];

const SCAN_DIRS: &[&str] = &["docs/architecture"];

#[test]
fn living_docs_reference_existing_paths() {
    let mut files: Vec<String> = Vec::new();
    for f in SCAN_FILES {
        if Path::new(f).exists() {
            files.push(f.to_string());
        }
    }
    for dir in SCAN_DIRS {
        collect_md(Path::new(dir), &mut files);
    }
    assert!(!files.is_empty(), "no living docs found to scan");

    let mut bad: Vec<String> = Vec::new();
    for f in &files {
        let text = fs::read_to_string(f).unwrap_or_else(|e| panic!("read {f}: {e}"));
        // Intentional historical mentions: the legacy `src/agent.rs` split and
        // frozen session-log excerpts quoted inside layer3-episodic.md.
        const ALLOWED: &[&str] = &["src/agent.rs", "src/permissions.rs"];
        for p in extract_repo_paths(&text) {
            if ALLOWED.contains(&p.as_str()) {
                continue;
            }
            if !Path::new(&p).exists() {
                bad.push(format!("{f}: `{p}` does not exist"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "living docs reference non-existent paths (run the guard to see them; \
         when moving a file, grep living docs for the old path):\n{}",
        bad.join("\n")
    );
}

fn collect_md(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_md(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p.to_string_lossy().into_owned());
        }
    }
}

/// Extract `src/...`, `crates/...`, `tests/...`, `e2e/...` path references
/// ending in a Rust-ish file or directory, tolerating `:line` / `::item`
/// suffixes.
fn extract_repo_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let prefixes = ["src/", "crates/", "tests/", "e2e/", ".dev/scripts/"];
    for (i, _) in text.char_indices() {
        let rest = &text[i..];
        let Some(pref) = prefixes.iter().find(|p| rest.starts_with(**p)) else {
            continue;
        };
        // Path boundary: skip matches embedded in a longer path
        // (e.g. the `src/ui/chat.rs` inside `crates/recursive-tui/src/ui/chat.rs`).
        if i > 0 {
            let prev = bytes[i - 1];
            if prev == b'/'
                || prev == b'_'
                || prev == b'.'
                || prev.is_ascii_alphanumeric()
                || prev == b'-'
            {
                continue;
            }
        }
        let mut end = i + pref.len();
        for c in rest[pref.len()..].chars() {
            if c.is_ascii_alphanumeric()
                || c == '_'
                || c == '/'
                || c == '.'
                || c == '-'
                || c == '{'
                || c == '}'
            {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        let raw = &text[i..end];
        // strip trailing punctuation / brace-groups
        let candidate = raw.trim_end_matches(['.', ',', ';', ')']);
        if candidate.contains('{') {
            continue;
        }
        // Only guard Rust-ish targets to avoid prose false positives.
        if candidate.ends_with(".rs") || candidate.ends_with("/src") {
            out.push(candidate.to_string());
        }
    }
    out
}

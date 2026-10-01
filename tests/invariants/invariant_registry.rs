// Why this test exists:
// .dev/AGENTS.md invariants #9/#10: "New tool → new file" (registered in
// src/tools/mod.rs) and "New provider → new file + trait" (implements
// ChatProvider, registered in src/llm/mod.rs).
//
// Additionally guards the *numbering* of the invariants themselves: the
// number → title mapping in `.dev/AGENTS.md` and
// `docs/architecture/invariants.md` must agree. These documents are the
// rollback criteria of the self-improve loop; a mismatch means "violated
// invariant #N" points at different rules depending on which document an
// agent read (issue #61).

use std::collections::BTreeMap;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("{rel} must be readable in the repo: {e}"))
}

// ── Invariant #9: new tool → new file under src/tools/ ─────────────────────

/// Every `*.rs` file in `src/tools/` that declares `impl Tool` (or a Tool
/// impl block) must be declared as a module in `src/tools/mod.rs`. This is
/// the mechanical core of "new tool → new file, registered in mod.rs".
#[test]
fn tool_files_are_registered_in_mod_rs() {
    let mod_rs = read("src/tools/mod.rs");
    let dir = workspace_root().join("src/tools");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("src/tools must be listable: {e}"));
    let mut checked = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".rs") || name == "mod.rs" {
            continue;
        }
        let stem = name.trim_end_matches(".rs");
        let content = std::fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", name));
        if !content.contains("Tool for") && !content.contains("impl Tool") {
            continue; // helper modules without tool impls are fine
        }
        assert!(
            mod_rs.contains(&format!("mod {stem};")),
            "invariant #9 violation: src/tools/{name} implements a Tool but is not \
             declared in src/tools/mod.rs"
        );
        checked += 1;
    }
    assert!(
        checked >= 20,
        "expected to check many tool files, got {checked}"
    );
}

// ── Invariant #10: new provider → new file implementing ChatProvider ───────

/// Any file in `src/llm/` that implements `ChatProvider` must be declared as
/// a module in `src/llm/mod.rs`, and no file outside `src/llm/` (or tests)
/// implements `ChatProvider`.
#[test]
fn providers_live_in_llm_and_implement_the_trait() {
    let llm_mod = read("src/llm/mod.rs");
    let dir = workspace_root().join("src/llm");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("src/llm must be listable: {e}"));
    let mut found = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".rs") || name == "mod.rs" {
            continue;
        }
        let stem = name.trim_end_matches(".rs");
        let content = std::fs::read_to_string(entry.path())
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", name));
        if !content.contains("impl ChatProvider") {
            continue;
        }
        assert!(
            llm_mod.contains(&format!("mod {stem};")),
            "invariant #10 violation: src/llm/{name} implements ChatProvider but is \
             not declared in src/llm/mod.rs"
        );
        found += 1;
    }
    assert!(found >= 2, "expected ≥2 provider files, found {found}");

    // Provider implementations must not leak outside src/llm/.
    let src = workspace_root().join("src");
    let mut stack = vec![src.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .unwrap_or_else(|e| panic!("{} must be listable: {e}", d.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            if path.starts_with(&dir) {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            // Test mocks (`#[cfg(test)]` impls) are out of scope.
            let product_code = match content.find("#[cfg(test)]") {
                Some(idx) => &content[..idx],
                None => &content,
            };
            assert!(
                !product_code.contains("impl ChatProvider"),
                "invariant #10 violation: {} implements ChatProvider outside \
                 src/llm/",
                path.strip_prefix(&src).unwrap_or(&path).display()
            );
        }
    }
}

// ── Numbering guard: .dev/AGENTS.md ↔ docs/architecture/invariants.md ──────

/// Parse `N. **Title.**` list items from the .dev/AGENTS.md invariants
/// section into (N → normalized title).
fn parse_dev_numbering(text: &str) -> BTreeMap<u32, String> {
    let section = text
        .split("## Invariants (DO NOT BREAK)")
        .nth(1)
        .and_then(|s| s.split("## ").next())
        .expect(".dev/AGENTS.md must have an '## Invariants (DO NOT BREAK)' section");
    let mut map = BTreeMap::new();
    for line in section.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        // match "N. **Title" possibly with leading digits (e.g. "10.")
        let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Some(rest) = trimmed.strip_prefix(&digits) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(". **") else {
            continue;
        };
        let Some(title) = rest.split(".**").next() else {
            continue;
        };
        let num: u32 = digits.parse().expect("invariant number must parse");
        map.insert(num, normalize_title(title));
    }
    map
}

/// Parse `## Invariant #N — Title` headings from the architecture doc.
fn parse_docs_numbering(text: &str) -> BTreeMap<u32, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("## Invariant #") else {
            continue;
        };
        let Some((num, title)) = rest.split_once(" — ") else {
            continue;
        };
        let Ok(num) = num.parse::<u32>() else {
            continue;
        };
        map.insert(num, normalize_title(title));
    }
    map
}

fn normalize_title(t: &str) -> String {
    t.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// The numbered invariants in `.dev/AGENTS.md` and
/// `docs/architecture/invariants.md` must carry the identical
/// (number → title) mapping. Both documents are rollback criteria; a
/// mismatch means "violated invariant #N" is ambiguous (issue #61).
#[test]
fn invariant_numbering_agrees_across_documents() {
    let dev = parse_dev_numbering(&read(".dev/AGENTS.md"));
    let docs = parse_docs_numbering(&read("docs/architecture/invariants.md"));
    assert!(
        !dev.is_empty(),
        "failed to parse invariants from .dev/AGENTS.md"
    );
    assert_eq!(
        dev, docs,
        "invariant numbering mismatch between .dev/AGENTS.md and \
         docs/architecture/invariants.md — update both (and this test) together"
    );
}

/// Every numbered invariant in `.dev/AGENTS.md` must either name an
/// automated test or explicitly state that it is enforced by clippy — no
/// silent, untested invariants.
#[test]
fn every_numbered_invariant_has_automated_enforcement() {
    let text = read(".dev/AGENTS.md");
    let section = text
        .split("## Invariants (DO NOT BREAK)")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .expect("invariants section");
    let mut numbers = Vec::new();
    for line in section.lines() {
        let t = line.trim_start();
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if t.strip_prefix(&digits)
            .is_some_and(|r| r.starts_with(". **"))
        {
            if let Ok(n) = digits.parse::<u32>() {
                numbers.push(n);
            }
        }
    }
    // Each numbered entry spans until the next number; require an
    // "Automated test:" or "Enforced by:" line in its body.
    for (i, n) in numbers.iter().enumerate() {
        let start = section.find(&format!("\n{}.", n)).unwrap_or(usize::MAX);
        let end = numbers
            .get(i + 1)
            .and_then(|next| section.find(&format!("\n{}.", next)))
            .unwrap_or(section.len());
        let body = &section[start..end];
        assert!(
            body.contains("Automated test:") || body.contains("Enforced by:"),
            "invariant #{n} in .dev/AGENTS.md has no automated enforcement listed"
        );
    }
}

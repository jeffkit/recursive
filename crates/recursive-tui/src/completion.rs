//! Tab-completion and history-search helpers for the Recursive TUI.
//!
//! Provides [`glob_workspace_files`] for `@`-file completion, [`search_history`]
//! for Ctrl+R fuzzy history search, and [`default_offline_tool_catalog`] for the
//! static tool list shown when the runtime is unavailable.

// ──────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────

/// Maximum candidates shown in the @file popup.
pub const MAX_ATFILE_SUGGESTIONS: usize = 12;

/// Maximum results shown in the Ctrl+R history-search popup.
pub const MAX_HSEARCH_RESULTS: usize = 12;

// ──────────────────────────────────────────────────────────────────────
// Tool catalog
// ──────────────────────────────────────────────────────────────────────

/// Static fallback list of tools shown by `/tools` when the TUI is
/// running in offline mode (no runtime to query). Mirrors the set
/// `backend::build_default_tools` registers.
pub fn default_offline_tool_catalog() -> Vec<(String, String)> {
    vec![
        ("Read".into(), "Read a file from the workspace".into()),
        ("Write".into(), "Write a file to the workspace".into()),
        (
            "Edit".into(),
            "Edit a file with exact string replacement".into(),
        ),
        ("Bash".into(), "Run a shell command in the workspace".into()),
        ("Grep".into(), "Search files for a regex pattern".into()),
        ("Glob".into(), "Find files matching a glob pattern".into()),
    ]
}

// ──────────────────────────────────────────────────────────────────────
// History search (Goal 160)
// ──────────────────────────────────────────────────────────────────────

/// Fuzzy-search `history` for `query` (case-insensitive substring match).
///
/// Returns indices into `history` ordered by relevance: prefix matches come
/// before substring matches. When `query` is empty, returns all indices in
/// reverse insertion order (most-recent first). Results are capped at
/// [`MAX_HSEARCH_RESULTS`].
pub fn search_history(history: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        // All entries, most recent first.
        let mut all: Vec<usize> = (0..history.len()).rev().collect();
        all.truncate(MAX_HSEARCH_RESULTS);
        return all;
    }
    let mut prefix: Vec<usize> = Vec::new();
    let mut substr: Vec<usize> = Vec::new();
    for (i, entry) in history.iter().enumerate() {
        let lower = entry.to_lowercase();
        if lower.starts_with(&q) {
            prefix.push(i);
        } else if lower.contains(&q) {
            substr.push(i);
        }
    }
    // Most recent prefix matches first, then most recent substr matches.
    prefix.reverse();
    substr.reverse();
    let mut out = prefix;
    out.extend(substr);
    out.truncate(MAX_HSEARCH_RESULTS);
    out
}

// ──────────────────────────────────────────────────────────────────────
// @file autocomplete (Goal 158)
// ──────────────────────────────────────────────────────────────────────

/// Enumerate workspace files matching `query` (case-insensitive prefix /
/// substring match). Returns relative paths, newest-first within each
/// depth tier, capped at [`MAX_ATFILE_SUGGESTIONS`].
///
/// Excludes: `target/`, `.git/`, `node_modules/`. Walks at most 3
/// directory levels deep so the function stays fast even in large trees.
pub fn glob_workspace_files(query: &str) -> Vec<String> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let q = query.to_lowercase();
    let mut results: Vec<String> = Vec::new();
    collect_files(&cwd, &cwd, 0, &q, &mut results);
    results.sort();
    results.dedup();
    // Prefer entries whose filename starts with the query.
    results.sort_by_key(|p| {
        let name = std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        if name.starts_with(&q) {
            0u8
        } else {
            1u8
        }
    });
    results.truncate(MAX_ATFILE_SUGGESTIONS);
    results
}

pub fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    query: &str,
    out: &mut Vec<String>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs and common large dirs.
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, depth + 1, query, out);
        } else if path.is_file() {
            // Normalize to forward slashes so relative paths are stable across
            // platforms (Windows `strip_prefix` yields `d1\d2\l3.txt`, but
            // callers and tests expect `d1/d2/l3.txt`).
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name_str.to_string());
            if (query.is_empty() || rel.to_lowercase().contains(query))
                && out.len() < MAX_ATFILE_SUGGESTIONS * 4
            {
                out.push(rel);
            }
        }
    }
}

#[cfg(test)]
mod debt_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write(root: &std::path::Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, b"x").unwrap();
        p
    }

    // ── default_offline_tool_catalog (25) ───────────────────────────────

    #[test]
    fn default_offline_tool_catalog_has_six_named_entries() {
        // kills all three 25:5 mutants:
        //  - vec![]                -> len 0
        //  - vec![(empty, empty)]  -> len 1 + empty name
        //  - vec![(empty, "xyzzy")] -> len 1 + empty name
        let cat = default_offline_tool_catalog();
        assert_eq!(cat.len(), 6, "expected 6 tools, got {} entries", cat.len());
        assert!(
            cat.iter().all(|(n, _)| !n.is_empty()),
            "every tool name must be non-empty; got {cat:?}"
        );
        assert_eq!(cat[0].0, "Read");
    }

    // ── glob_workspace_files (86) ───────────────────────────────────────

    #[test]
    fn glob_workspace_files_finds_cargo_toml_in_cwd() {
        // kills glob_workspace_files -> vec![] (86): the mutant returns an
        // empty vec regardless of query, so it would never contain
        // "Cargo.toml". Runs in the crate's worktree which always has a
        // root Cargo.toml.
        let results = glob_workspace_files("cargo");
        assert!(
            results.iter().any(|p| p.contains("Cargo.toml")),
            "expected Cargo.toml among results, got {results:?}"
        );
    }

    // ── collect_files -> () (118:5) ─────────────────────────────────────

    #[test]
    fn collect_files_populates_out_vec() {
        // kills collect_files -> () (118:5).
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.txt");
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "", &mut out);
        assert_eq!(out, vec!["a.txt".to_string()]);
    }

    // ── depth guard (118:14) + depth increment (133:46) ─────────────────

    #[test]
    fn collect_files_walks_four_levels_deep() {
        // 5 nested files at depths 1..5. orig walks while depth <= 3, so it
        // collects l1/l2/l3 but NOT l4 or deep (the depth-4 dir is never
        // recursed into because `depth + 1 == 4 > 3`).
        // Kills 118:14 `>`->`==`/`>=` (depth-3 returns early -> l3 missing),
        // 118:14 `>`->`<` (depth-0 returns early -> l1 missing), and 133:46
        // `+`->`*` (depth never grows -> walks arbitrarily deep -> deep.txt
        // present).
        let dir = TempDir::new().unwrap();
        write(dir.path(), "d1/l1.txt");
        write(dir.path(), "d1/d2/l2.txt");
        write(dir.path(), "d1/d2/d3/l3.txt");
        write(dir.path(), "d1/d2/d3/d4/l4.txt");
        write(dir.path(), "d1/d2/d3/d4/deep.txt");
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "", &mut out);
        let mut sorted = out.clone();
        sorted.sort();
        assert!(
            sorted.contains(&"d1/l1.txt".to_string()),
            "depth-1 file should be collected; got {sorted:?}"
        );
        assert!(
            sorted.contains(&"d1/d2/d3/l3.txt".to_string()),
            "depth-3 file should be collected; got {sorted:?}"
        );
        assert!(
            !sorted.contains(&"d1/d2/d3/d4/deep.txt".to_string()),
            "depth-5 file should NOT be collected (depth>3); got {sorted:?}"
        );
        assert!(
            !sorted.contains(&"d1/d2/d3/d4/l4.txt".to_string()),
            "depth-4 file should NOT be collected; got {sorted:?}"
        );
    }

    // ── path separator normalization (135:137) ───────────────────────────

    #[test]
    fn collect_files_emits_forward_slash_relative_paths() {
        // Pins the cross-platform contract: relative paths returned by
        // `collect_files` always use forward slashes, even on Windows where
        // `Path::strip_prefix(..).to_string_lossy()` would otherwise yield
        // `d1\d2\file.txt`. Without normalization the `@`-completion menu and
        // any path-based assertion would be OS-dependent.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "d1/d2/deep.txt");
        write(dir.path(), "d1/d2/d3/nested.txt");
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "", &mut out);
        assert!(
            out.iter().all(|p| !p.contains('\\')),
            "no backslash separators expected; got {out:?}"
        );
        assert!(
            out.contains(&"d1/d2/deep.txt".to_string()),
            "expected forward-slash rel path; got {out:?}"
        );
        assert!(
            out.contains(&"d1/d2/d3/nested.txt".to_string()),
            "expected forward-slash rel path; got {out:?}"
        );
    }

    // ── skip predicates (129) ───────────────────────────────────────────

    #[test]
    fn collect_files_skips_hidden_target_and_node_modules() {
        // kills all three 129 mutants:
        //  - 129:50 `starts_with('.') ==`->`!=` : enters ".hidden"
        //  - 129:74 `== "target"`->`!=`         : enters "target"
        //  - 129:62 `||`->`&&`                  : enters every excluded dir
        // Under orig the files inside these dirs are never collected.
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".hidden/secret.txt");
        write(dir.path(), "target/build.txt");
        write(dir.path(), "node_modules/pkg.txt");
        write(dir.path(), "keep.txt");
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "", &mut out);
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["keep.txt".to_string()]);
    }

    // ── query filter (139:34) ───────────────────────────────────────────

    #[test]
    fn collect_files_nonempty_query_matches_substring() {
        // kills 139:34 `||`->`&&`: with a non-empty query the mutant becomes
        // `query.is_empty() && rel.contains(query)` which is always false,
        // so nothing is collected.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "alpha.rs");
        write(dir.path(), "beta.rs");
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "alph", &mut out);
        assert_eq!(out, vec!["alpha.rs".to_string()]);
    }

    // ── out-len cap (140:30, 140:55) ────────────────────────────────────

    #[test]
    fn collect_files_caps_at_four_times_max_suggestions() {
        // 49 files, empty query -> orig collects until out.len() reaches
        // MAX_ATFILE_SUGGESTIONS * 4 == 48, then stops. So exactly 48.
        // Kills 140:30 `<`->`==`/`>` (collects 0) and `<=` (collects 49),
        // and 140:55 `*`->`/` (cap becomes 12/4 == 3 -> collects 3).
        let dir = TempDir::new().unwrap();
        for i in 0..49 {
            write(dir.path(), &format!("f{i:02}.txt"));
        }
        let mut out: Vec<String> = Vec::new();
        collect_files(dir.path(), dir.path(), 0, "", &mut out);
        assert_eq!(
            out.len(),
            MAX_ATFILE_SUGGESTIONS * 4,
            "expected exactly {} entries, got {}",
            MAX_ATFILE_SUGGESTIONS * 4,
            out.len()
        );
    }
}

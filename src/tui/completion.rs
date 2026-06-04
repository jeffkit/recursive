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
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| name_str.to_string());
            if (query.is_empty() || rel.to_lowercase().contains(query))
                && out.len() < MAX_ATFILE_SUGGESTIONS * 4
            {
                out.push(rel);
            }
        }
    }
}

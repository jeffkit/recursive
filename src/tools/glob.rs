//! `Glob` tool: find files matching a glob pattern inside the workspace.
//!
//! Uses `walkdir` (already in Cargo.toml) to walk the directory tree and
//! matches entries using a simple built-in glob matcher that supports
//! `*` (any characters within a single path component), `**` (any number
//! of path components), and `?` (exactly one character).
//!
//! No extra crate dependency is needed.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use walkdir::WalkDir;

use super::{resolve_within_any, AccessTier, SharedSandboxRoots, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const MAX_RESULTS: usize = 200;

/// Match a single path component against a glob segment.
/// Supports `*` (zero or more chars) and `?` (exactly one char).
/// Does NOT support `**` at this level — that is handled by the outer
/// walk logic.
fn match_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_seg_inner(&p, &t)
}

fn match_seg_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some(&'*'), _) => {
            // `*` in a segment matches zero or more chars (within the segment)
            // Try skipping zero, one, ... chars in t.
            if match_seg_inner(&p[1..], t) {
                return true;
            }
            if !t.is_empty() {
                return match_seg_inner(p, &t[1..]);
            }
            false
        }
        (Some(&'?'), Some(_)) => match_seg_inner(&p[1..], &t[1..]),
        (Some(pc), Some(tc)) if pc == tc => match_seg_inner(&p[1..], &t[1..]),
        _ => false,
    }
}

/// Match a path (given as its components) against a glob pattern split on `/`.
///
/// `**` in the pattern matches zero or more path components.
fn match_path(pattern_parts: &[&str], path_parts: &[&str]) -> bool {
    match (pattern_parts.first(), path_parts.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&"**"), _) => {
            // `**` matches zero components
            if match_path(&pattern_parts[1..], path_parts) {
                return true;
            }
            // or consume one component and try again
            if !path_parts.is_empty() {
                return match_path(pattern_parts, &path_parts[1..]);
            }
            false
        }
        (_, None) => false,
        (Some(pp), Some(tp)) => {
            if match_segment(pp, tp) {
                match_path(&pattern_parts[1..], &path_parts[1..])
            } else {
                false
            }
        }
    }
}

/// Test if a workspace-relative path string matches a glob pattern string.
fn glob_matches(pattern: &str, rel_path: &str) -> bool {
    // Normalise to forward slashes on all platforms.
    let rel = rel_path.replace('\\', "/");
    let pat = pattern.replace('\\', "/");
    let pattern_parts: Vec<&str> = pat.split('/').collect();
    let path_parts: Vec<&str> = rel.split('/').collect();
    match_path(&pattern_parts, &path_parts)
}

/// The `Glob` tool.
#[derive(Debug, Clone)]
pub struct GlobTool {
    pub root: PathBuf,
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    pub session_roots: Option<SharedSandboxRoots>,
}

impl GlobTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
        }
    }

    /// Append additional allowed sandbox roots. See
    /// [`crate::tools::fs::ReadFile::with_extra_roots`].
    pub fn with_extra_roots(
        mut self,
        extra: impl IntoIterator<Item = (PathBuf, AccessTier)>,
    ) -> Self {
        self.extra_roots.extend(extra);
        self
    }

    /// Attach the shared, session-mutable roots slot. See [`SharedSandboxRoots`].
    pub fn with_session_roots(mut self, slot: SharedSandboxRoots) -> Self {
        self.session_roots = Some(slot);
        self
    }

    /// Convenience: attach the shared slot only when `Some`.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    fn all_roots(&self) -> Vec<(PathBuf, AccessTier)> {
        let mut v: Vec<(PathBuf, AccessTier)> = Vec::with_capacity(self.extra_roots.len() + 1);
        v.push((self.root.clone(), AccessTier::ReadWrite));
        v.extend(self.extra_roots.iter().cloned());
        if let Some(slot) = &self.session_roots {
            if let Ok(roots) = slot.read() {
                v.extend(roots.iter().cloned());
            }
        }
        v
    }

    /// Render `entry_path` relative to the primary root when possible, else
    /// relative to any matching extra root, else as-is (absolute). This keeps
    /// workspace hits workspace-relative while extra-root hits become absolute
    /// so the agent can feed them back to `Read`.
    fn relativise(&self, entry_path: &std::path::Path) -> String {
        if let Ok(rel) = entry_path.strip_prefix(&self.root) {
            return rel.to_string_lossy().into_owned();
        }
        // Hits inside an extra root are reported as absolute paths so the
        // agent can feed them straight back to `Read` (which accepts
        // absolute paths under any allowed root).
        if self
            .extra_roots
            .iter()
            .any(|(extra, _)| entry_path.starts_with(extra))
        {
            return entry_path.to_string_lossy().into_owned();
        }
        entry_path.to_string_lossy().into_owned()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Glob".into(),
            description: "Find files matching a glob pattern (e.g. \"**/*.rs\") inside the workspace. Returns matching paths relative to the workspace root, one per line. Capped at 200 results.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match. Supports `*` (within a path component), `**` (any number of components), and `?` (single character). Example: \"src/**/*.rs\""
                    },
                    "path": {
                        "type": "string",
                        "description": "Optional workspace-relative subdirectory to scope the search. Defaults to the workspace root."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Glob".into(),
            message: "missing `pattern`".into(),
        })?;

        if pattern.is_empty() {
            return Err(Error::BadToolArgs {
                name: "Glob".into(),
                message: "`pattern` must not be empty".into(),
            });
        }

        let scope = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => {
                resolve_within_any(&self.all_roots(), p, false).map_err(|e| Error::BadToolArgs {
                    name: "Glob".into(),
                    message: format!("path: {e}"),
                })?
            }
            None => self.root.clone(),
        };

        let mut matches: Vec<String> = Vec::new();

        for entry in WalkDir::new(&scope)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel = self.relativise(entry.path());

            if glob_matches(pattern, &rel) {
                matches.push(rel);
                if matches.len() >= MAX_RESULTS {
                    break;
                }
            }
        }

        matches.sort();

        if matches.is_empty() {
            Ok(format!("no files matching `{pattern}`"))
        } else {
            Ok(matches.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_files(dir: &TempDir, names: &[&str]) {
        for name in names {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, "x").unwrap();
        }
    }

    #[test]
    fn glob_matches_extension_wildcard() {
        assert!(glob_matches("**/*.rs", "src/lib.rs"));
        assert!(glob_matches("**/*.rs", "src/tools/mod.rs"));
        assert!(!glob_matches("**/*.rs", "src/lib.txt"));
    }

    #[test]
    fn glob_matches_single_star() {
        assert!(glob_matches("src/*.rs", "src/lib.rs"));
        assert!(!glob_matches("src/*.rs", "src/tools/mod.rs")); // nested, not matching
    }

    #[test]
    fn glob_matches_question_mark() {
        assert!(glob_matches("src/?.rs", "src/a.rs"));
        assert!(!glob_matches("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn glob_matches_double_star_zero_components() {
        // `**` can match zero components
        assert!(glob_matches("**", "foo.rs"));
        assert!(glob_matches("**/*.rs", "lib.rs"));
    }

    #[tokio::test]
    async fn finds_rs_files_by_extension() {
        let tmp = TempDir::new().unwrap();
        create_files(&tmp, &["src/a.rs", "src/b.rs", "src/c.txt"]);
        let tool = GlobTool::new(tmp.path());
        let out = tool.execute(json!({"pattern": "**/*.rs"})).await.unwrap();
        assert!(out.contains("a.rs"));
        assert!(out.contains("b.rs"));
        assert!(!out.contains("c.txt"));
    }

    #[tokio::test]
    async fn scope_by_path_restricts_results() {
        let tmp = TempDir::new().unwrap();
        create_files(&tmp, &["src/a.rs", "tests/b.rs"]);
        let tool = GlobTool::new(tmp.path());
        let out = tool
            .execute(json!({"pattern": "**/*.rs", "path": "src"}))
            .await
            .unwrap();
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.rs"));
    }

    #[tokio::test]
    async fn no_matches_returns_informative_message() {
        let tmp = TempDir::new().unwrap();
        create_files(&tmp, &["foo.txt"]);
        let tool = GlobTool::new(tmp.path());
        let out = tool.execute(json!({"pattern": "**/*.rs"})).await.unwrap();
        assert!(out.contains("no files matching"));
    }

    #[tokio::test]
    async fn cap_at_200_results() {
        let tmp = TempDir::new().unwrap();
        // Create 210 .rs files
        for i in 0..210usize {
            std::fs::write(tmp.path().join(format!("f{i}.rs")), "x").unwrap();
        }
        let tool = GlobTool::new(tmp.path());
        let out = tool.execute(json!({"pattern": "*.rs"})).await.unwrap();
        assert_eq!(out.lines().count(), 200);
    }

    #[tokio::test]
    async fn empty_pattern_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = GlobTool::new(tmp.path());
        let err = tool.execute(json!({"pattern": ""})).await.unwrap_err();
        assert!(matches!(err, crate::error::Error::BadToolArgs { .. }));
    }

    // ── match_segment / match_seg_inner direct tests ─────────────────────────

    #[test]
    fn match_segment_exact_match_true() {
        // kills `match_seg_inner -> bool with false`
        assert!(match_segment("foo", "foo"), "exact match must be true");
        assert!(match_segment("", ""), "empty vs empty must be true (None,None arm)");
    }

    #[test]
    fn match_segment_literal_mismatch_false() {
        // kills `match_segment -> bool with true`
        assert!(!match_segment("foo", "bar"), "literal mismatch must be false");
        assert!(!match_segment("a", "b"), "single-char mismatch must be false");
    }

    #[test]
    fn match_segment_star_matches_multi_chars() {
        // kills `delete match arm (Some(&'*'), _) in match_seg_inner`
        assert!(match_segment("*", "anything"), "* must match non-empty text");
        assert!(match_segment("*", ""), "* must match empty text (zero chars)");
        assert!(match_segment("*.rs", "lib.rs"), "*.rs must match lib.rs");
        assert!(!match_segment("*.rs", "lib.txt"), "*.rs must not match lib.txt");
    }

    #[test]
    fn match_segment_star_consume_chars_delete_not() {
        // Specifically targets `delete ! in match_seg_inner` line 40.
        // `*a` must match "ba" — requires * to skip the leading 'b'.
        // With `delete !`, the `if t.is_empty()` branch never recurses when t
        // is non-empty, so "ba" would fail to match.
        assert!(match_segment("*a", "ba"), "*a must match 'ba'");
        assert!(match_segment("*a", "xxa"), "*a must match 'xxa'");
        assert!(!match_segment("*a", "bx"), "*a must not match 'bx'");
    }

    #[test]
    fn match_segment_question_marks() {
        // kills `delete match arm (Some(&'?'), Some(_)) in match_seg_inner`
        assert!(match_segment("?", "a"), "? must match exactly one char");
        assert!(!match_segment("?", ""), "? must not match empty text");
        assert!(!match_segment("?", "ab"), "? must not match two chars");
        assert!(match_segment("a?c", "abc"), "a?c must match 'abc'");
    }

    #[test]
    fn match_segment_guard_pc_eq_tc() {
        // kills `replace match guard pc == tc with true`
        // and `replace == with != in match_seg_inner`
        assert!(!match_segment("a", "b"), "non-equal chars must not match");
        assert!(match_segment("az", "az"), "equal chars must match");
    }

    // ── match_path / glob_matches direct tests ────────────────────────────────

    #[test]
    fn glob_matches_exact_literal_path() {
        // kills `match_path -> bool with false` and `glob_matches -> bool with false`
        assert!(glob_matches("src/lib.rs", "src/lib.rs"), "exact path must match");
    }

    #[test]
    fn glob_matches_literal_mismatch_false() {
        // kills `match_path -> bool with true` and `glob_matches -> bool with true`
        assert!(!glob_matches("src/foo.rs", "src/bar.rs"), "different filename must not match");
    }

    #[test]
    fn match_path_extra_component_false() {
        // (None, _) arm: pattern exhausted but path has more components
        assert!(!glob_matches("src/lib.rs", "src/lib.rs/extra"), "exhausted pattern with trailing component must be false");
        // (_, None) arm: path exhausted but pattern has more
        assert!(!glob_matches("src/lib.rs/extra", "src/lib.rs"), "trailing pattern component must be false");
    }

    #[test]
    fn glob_matches_double_star_consumes_components() {
        // kills `delete ! in match_path` line 64
        // ** must be able to consume one or more path components
        assert!(glob_matches("**/*.rs", "a/b/c/lib.rs"), "** must consume multiple components");
        assert!(!glob_matches("**/*.rs", "a/b/c/lib.txt"), "** must not match wrong extension");
    }

    // ── GlobTool::relativise ──────────────────────────────────────────────────

    #[test]
    fn relativise_strips_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let tool = GlobTool::new(tmp.path());
        let abs = tmp.path().join("src/lib.rs");
        let rel = tool.relativise(&abs);
        assert_eq!(rel, "src/lib.rs", "relativise must strip workspace root");
        assert!(
            !rel.starts_with(tmp.path().to_str().unwrap()),
            "result must not be absolute for workspace files"
        );
    }

    // ── GlobTool::all_roots + scope path via execute ───────────────────────────

    #[tokio::test]
    async fn scope_path_uses_all_roots_correctly() {
        let tmp = TempDir::new().unwrap();
        create_files(&tmp, &["sub/a.rs", "sub/b.txt"]);
        let tool = GlobTool::new(tmp.path());
        // with path arg → resolve_within_any(all_roots(), ...) must work
        let out = tool
            .execute(json!({"pattern": "**/*.rs", "path": "sub"}))
            .await
            .unwrap();
        assert!(out.contains("a.rs"), "sub/a.rs must appear in results");
        assert!(!out.contains("b.txt"), "b.txt must be filtered by pattern");
    }
}

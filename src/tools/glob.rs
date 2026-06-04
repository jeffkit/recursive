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

use super::{resolve_within, Tool};
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
}

impl GlobTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
            Some(p) => resolve_within(&self.root, p).map_err(|e| Error::BadToolArgs {
                name: "Glob".into(),
                message: format!("path: {e}"),
            })?,
            None => self.root.clone(),
        };

        let mut matches: Vec<String> = Vec::new();

        for entry in WalkDir::new(&scope)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();

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
}

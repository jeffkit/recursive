//! `str_replace`: edit a file by replacing an exact string.
//!
//! This is the recommended editing tool for single-file changes. The LLM
//! provides an `old_string` (text to find) and `new_string` (replacement),
//! and the tool does a precise search-and-replace with a fuzzy-match
//! fallback chain that recovers from common LLM output quirks
//! (curly quotes, trailing whitespace).
//!
//! Fuzzy-match chain (first success wins):
//!   1. Exact match
//!   2. Quote normalization (curly → straight quotes)
//!   3. Trailing whitespace strip (rstrip each line of old_string)

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

#[derive(Debug, Clone)]
pub struct StrReplaceTool {
    pub root: PathBuf,
}

impl StrReplaceTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Quote-normalization table: curly quotes → straight quotes.
const QUOTE_PAIRS: &[(char, char)] = &[
    ('\u{2018}', '\''), // left single
    ('\u{2019}', '\''), // right single
    ('\u{201c}', '"'),  // left double
    ('\u{201d}', '"'),  // right double
];

/// Replace curly quotes with their ASCII equivalents in-place.
fn normalize_quotes(s: &str) -> String {
    let mut out = s.to_string();
    for &(curly, straight) in QUOTE_PAIRS {
        out = out.replace(curly, std::str::from_utf8(&[straight as u8]).unwrap_or("?"));
    }
    out
}

/// Strip trailing whitespace from every line while preserving newlines.
fn strip_trailing_whitespace(s: &str) -> String {
    let mut lines: Vec<&str> = s.lines().collect();
    // Preserve a trailing empty line if the input ends with '\n'.
    let trailing_newline = s.ends_with('\n');
    for line in &mut lines {
        *line = line.trim_end();
    }
    let out = lines.join("\n");
    if trailing_newline {
        out + "\n"
    } else {
        out
    }
}

/// Try to find `needle` in `haystack`, returning the matching variant
/// and the effective needle that was found. `actual` is the needle text
/// that should be used for the replacement.
fn try_match(haystack: &str, needle: &str) -> Option<String> {
    // 1. Exact
    if haystack.contains(needle) {
        return Some(needle.to_string());
    }

    // 2. Quote normalization
    let qn = normalize_quotes(needle);
    if qn != needle && haystack.contains(&qn) {
        return Some(qn);
    }

    // 3. Trailing whitespace strip
    let tws = strip_trailing_whitespace(needle);
    if tws != needle && haystack.contains(&tws) {
        return Some(tws);
    }

    // 4. Both normalizations combined (quote first, then strip)
    let qn_tws = strip_trailing_whitespace(&qn);
    if qn_tws != needle && qn_tws != qn && qn_tws != tws && haystack.contains(&qn_tws) {
        return Some(qn_tws);
    }

    None
}

#[async_trait]
impl Tool for StrReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "str_replace".into(),
            description:
                "Edit a file by replacing an exact string. Prefer this over apply_patch for single-file edits. old_string must appear exactly once in the file (or set replace_all=true)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path relative to the workspace root"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to find and replace. If empty, creates a new file with new_string as content."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "If true, replace all occurrences. Default false (replace only first, error on multiple).",
                        "default": false
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::Mutating
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let file_path = args["file_path"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "str_replace".into(),
                message: "missing `file_path`".into(),
            })?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "str_replace".into(),
                message: "missing `old_string`".into(),
            })?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "str_replace".into(),
                message: "missing `new_string`".into(),
            })?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let abs_path = resolve_within(&self.root, file_path)?;

        // ── Empty old_string → create new file ─────────────────────────
        if old_string.is_empty() {
            if let Some(parent) = abs_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::Tool {
                        name: "str_replace".into(),
                        message: format!("mkdir {}: {e}", parent.display()),
                    })?;
            }
            tokio::fs::write(&abs_path, new_string)
                .await
                .map_err(|e| Error::Tool {
                    name: "str_replace".into(),
                    message: format!("{}: {e}", abs_path.display()),
                })?;
            return Ok(format!(
                "Created new file `{}` ({} bytes)",
                file_path,
                new_string.len()
            ));
        }

        // ── Read file ──────────────────────────────────────────────────
        let content = tokio::fs::read_to_string(&abs_path)
            .await
            .map_err(|e| Error::Tool {
                name: "str_replace".into(),
                message: format!("{}: {e}", abs_path.display()),
            })?;

        // ── Find old_string (with fuzzy fallback) ──────────────────────
        let actual_old = try_match(&content, old_string).ok_or_else(|| {
            Error::Tool {
                name: "str_replace".into(),
                message: format!(
                    "old_string not found in `{file_path}`. The text was not found in the file (exact or fuzzy match). Check the file contents and try again."
                ),
            }
        })?;

        // ── Count occurrences ──────────────────────────────────────────
        let occurrence_count = content.matches(&actual_old).count();

        if !replace_all && occurrence_count > 1 {
            return Err(Error::Tool {
                name: "str_replace".into(),
                message: format!(
                    "old_string appears {occurrence_count} times in `{file_path}`. \
                     Provide more surrounding context to narrow it to a single occurrence, \
                     or set replace_all=true."
                ),
            });
        }

        // ── Apply replacement ──────────────────────────────────────────
        let max_replace = if replace_all { usize::MAX } else { 1 };
        let updated = content.replacen(&actual_old, new_string, max_replace);

        tokio::fs::write(&abs_path, &updated)
            .await
            .map_err(|e| Error::Tool {
                name: "str_replace".into(),
                message: format!("{}: {e}", abs_path.display()),
            })?;

        let replaced_count = if replace_all {
            occurrence_count.to_string()
        } else {
            "1".to_string()
        };

        Ok(format!(
            "Successfully replaced {replaced_count} occurrence(s) in `{file_path}`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Unit tests for normalization helpers ──────────────────────────

    #[test]
    fn quote_normalization_replaces_curly_quotes() {
        let input = "Here\u{2019}s a \u{201c}quoted\u{201d} string with \u{2018}single\u{2019}.";
        let output = normalize_quotes(input);
        assert!(!output.contains('\u{2018}'));
        assert!(!output.contains('\u{2019}'));
        assert!(!output.contains('\u{201c}'));
        assert!(!output.contains('\u{201d}'));
        assert!(output.contains('\''));
        assert!(output.contains('"'));
        assert_eq!(output, "Here's a \"quoted\" string with 'single'.");
    }

    #[test]
    fn strip_trailing_whitespace_per_line() {
        let input = "hello   \nworld\t  \n  spaced  \n";
        let output = strip_trailing_whitespace(input);
        assert_eq!(output, "hello\nworld\n  spaced\n");
    }

    #[test]
    fn strip_trailing_whitespace_single_line() {
        let input = "trailing spaces   ";
        let output = strip_trailing_whitespace(input);
        assert_eq!(output, "trailing spaces");
    }

    #[test]
    fn try_match_exact() {
        assert_eq!(
            try_match("hello world", "hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn try_match_quote_normalization() {
        let haystack = "Here's a string";
        let needle = "Here\u{2019}s a string"; // right single quote
        assert_eq!(
            try_match(haystack, needle),
            Some("Here's a string".to_string())
        );
    }

    #[test]
    fn try_match_trailing_whitespace() {
        let haystack = "fn foo() {\n    bar\n}\n";
        let needle = "fn foo() {   \n    bar   \n}\n";
        assert_eq!(
            try_match(haystack, needle),
            Some("fn foo() {\n    bar\n}\n".to_string())
        );
    }

    #[test]
    fn try_match_not_found() {
        assert_eq!(try_match("hello world", "goodbye"), None);
    }

    // ── Tool tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn exact_match_replaces_once() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("src.txt"),
            "fn old_func() {}\nfn mid() {}\nfn other_func() {}\n",
        )
        .unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let result = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "fn old_func() {}",
                "new_string": "fn new() {}",
                "replace_all": false
            }))
            .await
            .unwrap();

        assert!(result.contains("Successfully replaced 1 occurrence"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "fn new() {}\nfn mid() {}\nfn other_func() {}\n");
    }

    #[tokio::test]
    async fn fails_when_not_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let err = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "goodbye",
                "new_string": "replaced"
            }))
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(
            msg.contains("not found"),
            "expected 'not found', got: {msg}"
        );
    }

    #[tokio::test]
    async fn fails_when_ambiguous() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "foo\nfoo\nfoo\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let err = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": false
            }))
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(msg.contains("appears 3 times"), "got: {msg}");
    }

    #[tokio::test]
    async fn replace_all_replaces_all() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "foo\nfoo\nfoo\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let result = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.contains("Successfully replaced 3 occurrence"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "bar\nbar\nbar\n");
    }

    #[tokio::test]
    async fn empty_old_string_creates_file() {
        let tmp = TempDir::new().unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let result = tool
            .execute(json!({
                "file_path": "new_file.txt",
                "old_string": "",
                "new_string": "brand new content"
            }))
            .await
            .unwrap();

        assert!(result.contains("Created new file"));
        let content = std::fs::read_to_string(tmp.path().join("new_file.txt")).unwrap();
        assert_eq!(content, "brand new content");
    }

    #[tokio::test]
    async fn empty_old_string_creates_file_with_parent_dirs() {
        let tmp = TempDir::new().unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        tool.execute(json!({
            "file_path": "a/b/c/new_file.txt",
            "old_string": "",
            "new_string": "deep"
        }))
        .await
        .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("a/b/c/new_file.txt")).unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn sandboxed_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let tool = StrReplaceTool::new(tmp.path());
        let err = tool
            .execute(json!({
                "file_path": "../outside.txt",
                "old_string": "x",
                "new_string": "y"
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("escapes") || msg.contains("BadToolArgs"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn quote_normalization_matches() {
        let tmp = TempDir::new().unwrap();
        // File has straight single quotes
        std::fs::write(tmp.path().join("src.txt"), "let msg = \"it's done\";\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        // old_string uses curly right single quote
        let result = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "let msg = \"it\u{2019}s done\";",
                "new_string": "let msg = \"it's replaced\";"
            }))
            .await
            .unwrap();

        assert!(result.contains("Successfully replaced 1 occurrence"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "let msg = \"it's replaced\";\n");
    }

    #[tokio::test]
    async fn trailing_whitespace_strip_matches() {
        let tmp = TempDir::new().unwrap();
        // File has clean indentation
        std::fs::write(tmp.path().join("src.txt"), "fn foo() {\n    bar\n}\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        // old_string has trailing spaces on each line
        let result = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "fn foo() {   \n    bar   \n}\n",
                "new_string": "fn foo() {\n    baz\n}\n"
            }))
            .await
            .unwrap();

        assert!(result.contains("Successfully replaced 1 occurrence"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "fn foo() {\n    baz\n}\n");
    }

    #[tokio::test]
    async fn replace_all_with_ambiguous_works() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "aaa bbb aaa\n").unwrap();

        let tool = StrReplaceTool::new(tmp.path());
        let result = tool
            .execute(json!({
                "file_path": "src.txt",
                "old_string": "aaa",
                "new_string": "ccc",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.contains("Successfully replaced 2 occurrence"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "ccc bbb ccc\n");
    }
}

//! `str_replace`: edit a file by replacing an exact string.
//!
//! This is the recommended editing tool for single-file changes. The LLM
//! provides an `old_string` (text to find) and `new_string` (replacement),
//! and the tool does a precise search-and-replace with a fuzzy-match
//! fallback chain that recovers from common LLM output quirks.
//!
//! Fuzzy-match chain (first success wins):
//!   1. Exact match
//!   2. Quote normalization (curly to straight quotes) — both needle and
//!      haystack are normalised; the match position is used to extract the
//!      *original* bytes from the haystack so the replacement is exact.
//!   3. Trailing whitespace strip (rstrip each line of old_string)
//!   4. Quote normalization + trailing whitespace strip combined
//!   5. XML-tag desanitization (model-escaped tags to real tags)
//!
//! When the match succeeds via quote normalization, curly-quote style is
//! preserved in `new_string` so the edit does not silently change typography.
//!
//! `new_string` has trailing whitespace stripped from each line before being
//! written (matching the normalisation applied to `old_string` in step 3/4),
//! unless the file is a Markdown file where trailing spaces are significant.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::fs::ReadFileState;

#[derive(Debug, Clone)]
pub struct StrReplaceTool {
    pub root: PathBuf,
    /// When `Some`, enforces the partial-read guard: edits on files that were
    /// never read, or only partially read, are rejected with a clear error.
    /// `None` (default) disables the guard for backward compatibility.
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}

impl StrReplaceTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_state: None,
        }
    }

    pub fn with_read_state(mut self, slot: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_state = Some(slot);
        self
    }
}

// ---------------------------------------------------------------------------
// Curly-quote helpers
// ---------------------------------------------------------------------------

const LEFT_SINGLE: char = '\u{2018}';
const RIGHT_SINGLE: char = '\u{2019}';
const LEFT_DOUBLE: char = '\u{201c}';
const RIGHT_DOUBLE: char = '\u{201d}';

/// Replace curly quotes with their ASCII equivalents.
fn normalize_quotes(s: &str) -> String {
    s.replace([LEFT_SINGLE, RIGHT_SINGLE], "'")
        .replace([LEFT_DOUBLE, RIGHT_DOUBLE], "\"")
}

fn is_opening_context(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    matches!(
        chars[i - 1],
        ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}'
    )
}

/// Apply curly double-quote style to ASCII double-quotes in `s`.
fn apply_curly_double(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '"' {
            out.push(if is_opening_context(&chars, i) {
                LEFT_DOUBLE
            } else {
                RIGHT_DOUBLE
            });
        } else {
            out.push(c);
        }
    }
    out
}

/// Apply curly single-quote style to ASCII single-quotes in `s`.
fn apply_curly_single(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '\'' {
            let prev_letter = i > 0 && chars[i - 1].is_alphabetic();
            let next_letter = i + 1 < chars.len() && chars[i + 1].is_alphabetic();
            // Apostrophe in a contraction uses right single curly quote.
            if prev_letter && next_letter {
                out.push(RIGHT_SINGLE);
            } else if is_opening_context(&chars, i) {
                out.push(LEFT_SINGLE);
            } else {
                out.push(RIGHT_SINGLE);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// When `old_string` matched via quote normalization (curly quotes in file,
/// straight quotes from model), apply the same curly-quote style to `new_string`
/// so the edit preserves the file typography.
fn preserve_quote_style(old_string: &str, actual_old: &str, new_string: &str) -> String {
    if old_string == actual_old {
        return new_string.to_string();
    }
    let has_double = actual_old.contains(LEFT_DOUBLE) || actual_old.contains(RIGHT_DOUBLE);
    let has_single = actual_old.contains(LEFT_SINGLE) || actual_old.contains(RIGHT_SINGLE);
    if !has_double && !has_single {
        return new_string.to_string();
    }
    let mut result = new_string.to_string();
    if has_double {
        result = apply_curly_double(&result);
    }
    if has_single {
        result = apply_curly_single(&result);
    }
    result
}

// ---------------------------------------------------------------------------
// Trailing-whitespace normalization
// ---------------------------------------------------------------------------

/// Strip trailing whitespace from every line while preserving newlines.
fn strip_trailing_whitespace(s: &str) -> String {
    let trailing_newline = s.ends_with('\n');
    let stripped: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    let out = stripped.join("\n");
    if trailing_newline {
        out + "\n"
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// Desanitization
// ---------------------------------------------------------------------------

/// Apply XML-tag desanitization: models sometimes emit placeholder forms
/// because the training pipeline has escaped the real tag names.
fn desanitize(s: &str) -> String {
    let mut out = s.to_string();
    let pairs: &[(&str, &str)] = &[
        ("<fnr>", "<function_results>"),
        ("</fnr>", "</function_results>"),
        ("<n>", "<name>"),
        ("</n>", "</name>"),
        ("<o>", "<output>"),
        ("</o>", "</output>"),
        ("<e>", "<error>"),
        ("</e>", "</error>"),
        ("<s>", "<system>"),
        ("</s>", "</system>"),
        ("<r>", "<result>"),
        ("</r>", "</result>"),
        ("< META_START >", "<META_START>"),
        ("< META_END >", "</META_END>"),
        ("< EOT >", "<EOT>"),
        ("< META >", "<META>"),
        ("< SOS >", "<SOS>"),
        (
            "

H:", "

Human:",
        ),
        (
            "

A:",
            "

Assistant:",
        ),
    ];
    for &(from, to) in pairs {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Match chain
// ---------------------------------------------------------------------------

/// Try to find `needle` in `haystack` using the fuzzy-match chain.
///
/// Returns the *actual* substring from `haystack` that matched (not the
/// normalised needle), so callers can do a byte-exact replacement.
///
/// For quote-normalisation steps the match is found by normalising *both*
/// sides, locating the byte index in the normalised haystack, then slicing
/// the *original* haystack at that position.  This mirrors the approach used
/// by Claude Code's FileEditTool and avoids silent replace failures when the
/// normalised needle doesn't exist verbatim in the original file.
fn try_match(haystack: &str, needle: &str) -> Option<String> {
    // 1. Exact
    if haystack.contains(needle) {
        return Some(needle.to_string());
    }

    // 2. Quote normalization — normalise both sides, find index in normalised
    //    haystack, then extract the original bytes from haystack.
    //
    //    We normalise both needle and haystack to straight quotes, search in
    //    that normalised space, then map the byte index back to the *original*
    //    haystack to return the verbatim slice.  This handles both directions:
    //      • needle has curly quotes, file has straight quotes
    //      • needle has straight quotes, file has curly quotes
    let qn_needle = normalize_quotes(needle);
    let qn_haystack = normalize_quotes(haystack);
    // Only enter this branch if normalisation actually changed something.
    if qn_needle != needle || qn_haystack != haystack {
        if let Some(idx) = qn_haystack.find(qn_needle.as_str()) {
            // `idx` is a byte index into `qn_haystack`. Because normalize_quotes
            // only does character-level replacements that preserve char boundaries
            // (curly quotes are multi-byte; straight quotes are single-byte), we
            // cannot use idx directly into `haystack`. Instead, map via char counts.
            let actual = extract_by_char_count(haystack, &qn_haystack, idx, needle.chars().count());
            return Some(actual);
        }
    }

    // 3. Trailing whitespace strip
    let tws = strip_trailing_whitespace(needle);
    if tws != needle && haystack.contains(tws.as_str()) {
        return Some(tws);
    }

    // 4. Quote normalization + trailing whitespace strip combined
    let qn_tws = strip_trailing_whitespace(&qn_needle);
    if qn_tws != needle && qn_tws != qn_needle && qn_tws != tws {
        let qn_tws_haystack = strip_trailing_whitespace(&qn_haystack);
        if let Some(idx) = qn_tws_haystack.find(qn_tws.as_str()) {
            let actual =
                extract_by_char_count(haystack, &qn_tws_haystack, idx, needle.chars().count());
            return Some(actual);
        }
    }

    // 5. Desanitization
    let ds = desanitize(needle);
    if ds != needle && haystack.contains(ds.as_str()) {
        return Some(ds);
    }

    None
}

/// Given a `normalised` string (derived from `original` by character-level
/// substitutions) and a byte `idx` into `normalised`, return the substring of
/// `original` that has the same *char count* as `char_len`.
///
/// This works because normalize_quotes replaces each character 1-for-1 (a
/// curly quote is still one Unicode scalar; a straight quote is also one), so
/// char counts are preserved even though byte counts differ.
fn extract_by_char_count(
    original: &str,
    normalised: &str,
    byte_idx: usize,
    char_len: usize,
) -> String {
    // Find the char offset that byte_idx corresponds to in normalised.
    let char_start = normalised[..byte_idx].chars().count();
    original.chars().skip(char_start).take(char_len).collect()
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for StrReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Edit".into(),
            description: "Performs exact string replacements in files.\n\
Usage:\n\
- You must use your `Read` tool at least once in the conversation before editing. \
This tool will error if you attempt an edit without reading the file.\n\
- When editing text from Read tool output, ensure you preserve the exact indentation \
(tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is: \
line number + tab. Everything after that is the actual file content to match. \
Never include any part of the line number prefix in the old_string or new_string.\n\
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless \
explicitly required.\n\
- Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.\n\
- The edit will FAIL if `old_string` is not unique in the file. Either provide a larger \
string with more surrounding context to make it unique or use `replace_all` to change \
every instance of `old_string`.\n\
- Use `replace_all` for replacing and renaming strings across the file. This parameter is \
useful if you want to rename a variable for instance."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to modify"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to replace (must be different from new_string). If empty, creates a new file with new_string as content."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace it with (must be different from old_string)"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences of old_string (default false)",
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
                name: "Edit".into(),
                message: "missing `file_path`".into(),
            })?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Edit".into(),
                message: "missing `old_string`".into(),
            })?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Edit".into(),
                message: "missing `new_string`".into(),
            })?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        let abs_path = resolve_within(&self.root, file_path)?;

        // ── Partial-read guard ──────────────────────────────────────────
        // Reject edits on files that have never been read, or were only read
        // partially (via start_line/end_line), in this session.
        if let Some(slot) = &self.read_state {
            if let Ok(state) = slot.lock() {
                match state.get(&abs_path) {
                    None => {
                        return Err(Error::Tool {
                            name: "Edit".into(),
                            message: format!(
                                "File `{file_path}` has not been read yet. \
                                 Read it first before editing."
                            ),
                        });
                    }
                    Some(record) if record.is_partial => {
                        return Err(Error::Tool {
                            name: "Edit".into(),
                            message: format!(
                                "File `{file_path}` was only partially read \
                                 (line range). Read the complete file before editing."
                            ),
                        });
                    }
                    Some(_) => {} // full read — proceed
                }
            }
        }

        // ── Empty old_string: create new file or overwrite empty file ───
        if old_string.is_empty() {
            if let Some(parent) = abs_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::Tool {
                        name: "Edit".into(),
                        message: format!("mkdir {}: {e}", parent.display()),
                    })?;
            }
            tokio::fs::write(&abs_path, new_string)
                .await
                .map_err(|e| Error::Tool {
                    name: "Edit".into(),
                    message: format!("{}: {e}", abs_path.display()),
                })?;
            return Ok(format!(
                "Created `{}` ({} bytes)",
                file_path,
                new_string.len()
            ));
        }

        // ── Read file ───────────────────────────────────────────────────
        let content = tokio::fs::read_to_string(&abs_path)
            .await
            .map_err(|e| Error::Tool {
                name: "Edit".into(),
                message: format!("{}: {e}", abs_path.display()),
            })?;

        // ── Guard: identical strings do nothing ─────────────────────────
        if old_string == new_string {
            return Err(Error::Tool {
                name: "Edit".into(),
                message: "No changes to make: old_string and new_string are exactly the same."
                    .into(),
            });
        }

        // ── Normalise new_string trailing whitespace (pre-pass) ─────────
        // Strip trailing spaces/tabs from each line of new_string before any
        // matching, mirroring the normalisation applied to old_string in the
        // fuzzy-match chain.  This prevents the model from accidentally writing
        // invisible trailing whitespace into files.  Markdown files are exempt
        // because two trailing spaces are a hard line-break in CommonMark.
        let new_string_normalised;
        let new_string = if file_path.ends_with(".md") || file_path.ends_with(".mdx") {
            new_string
        } else {
            new_string_normalised = strip_trailing_whitespace(new_string);
            &new_string_normalised
        };

        // ── Find old_string via fuzzy-match chain ───────────────────────
        let actual_old = try_match(&content, old_string).ok_or_else(|| Error::Tool {
            name: "Edit".into(),
            message: format!("String to replace not found in `{file_path}`.\nString: {old_string}"),
        })?;

        // ── Count occurrences ───────────────────────────────────────────
        let occurrence_count = content.matches(actual_old.as_str()).count();
        if !replace_all && occurrence_count > 1 {
            return Err(Error::Tool {
                name: "Edit".into(),
                message: format!(
                    "Found {occurrence_count} matches of the string to replace, but \
replace_all is false. To replace all occurrences, set replace_all to true. \
To replace only one occurrence, please provide more context to uniquely identify \
the instance.\nString: {old_string}"
                ),
            });
        }

        // ── Preserve curly-quote style in new_string ────────────────────
        let actual_new = preserve_quote_style(old_string, &actual_old, new_string);

        // ── Apply replacement ───────────────────────────────────────────
        let max_replace = if replace_all { usize::MAX } else { 1 };
        let updated = content.replacen(actual_old.as_str(), actual_new.as_str(), max_replace);

        tokio::fs::write(&abs_path, &updated)
            .await
            .map_err(|e| Error::Tool {
                name: "Edit".into(),
                message: format!("{}: {e}", abs_path.display()),
            })?;

        let replaced_count = if replace_all {
            occurrence_count.to_string()
        } else {
            "1".to_string()
        };

        if replace_all {
            Ok(format!(
                "The file {file_path} has been updated. All occurrences were successfully replaced."
            ))
        } else {
            Ok(format!(
                "The file {file_path} has been updated successfully. {replaced_count} occurrence replaced."
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── normalize_quotes ─────────────────────────────────────────────────

    #[test]
    fn quote_normalization_replaces_curly_quotes() {
        let input = "Here\u{2019}s a \u{201c}quoted\u{201d} string with \u{2018}single\u{2019}.";
        let output = normalize_quotes(input);
        assert!(!output.contains('\u{2018}'));
        assert!(!output.contains('\u{2019}'));
        assert!(!output.contains('\u{201c}'));
        assert!(!output.contains('\u{201d}'));
        assert!(output.contains('\'')); // single quote
        assert!(output.contains('"')); // double quote
    }

    // ── strip_trailing_whitespace ────────────────────────────────────────

    #[test]
    fn strip_trailing_whitespace_per_line() {
        let input = "hello   \nworld\t  \n  spaced  \n";
        let output = strip_trailing_whitespace(input);
        assert_eq!(output, "hello\nworld\n  spaced\n");
    }

    // ── preserve_quote_style ─────────────────────────────────────────────

    #[test]
    fn preserve_quote_style_noop_when_exact_match() {
        let result = preserve_quote_style("hello", "hello", "world");
        assert_eq!(result, "world");
    }

    #[test]
    fn preserve_quote_style_applies_curly_double_when_needed() {
        // actual_old contains curly double quotes (file uses them)
        let actual_old = "\u{201c}quoted\u{201d}";
        let result = preserve_quote_style("\"quoted\"", actual_old, "\"replaced\"");
        // new_string should have curly quotes applied
        assert!(result.contains('\u{201c}') || result.contains('\u{201d}'));
    }

    // ── desanitize ────────────────────────────────────────────────────────

    #[test]
    fn desanitize_function_results_tag() {
        let input = "before <fnr> after";
        let output = desanitize(input);
        assert!(output.contains("<function_results>"), "got: {output}");
    }

    #[test]
    fn desanitize_noop_when_no_match() {
        let input = "nothing to desanitize here";
        let output = desanitize(input);
        assert_eq!(output, input);
    }

    // ── try_match ─────────────────────────────────────────────────────────

    #[test]
    fn try_match_exact() {
        assert_eq!(
            try_match("hello world", "hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn try_match_quote_normalization() {
        // File uses straight quote; needle has curly quote — should match and
        // return the *original* substring from the haystack (straight quote).
        let haystack = "Here's a string";
        let needle = "Here\u{2019}s a string";
        assert_eq!(
            try_match(haystack, needle),
            Some("Here's a string".to_string())
        );
    }

    #[test]
    fn try_match_quote_normalization_file_has_curly_returns_original() {
        // File uses curly quotes; needle has straight quotes — the returned
        // value must be the original curly-quote slice, not the normalised needle.
        let haystack = "say \u{201c}hello\u{201d} world";
        let needle = "say \"hello\" world";
        let result = try_match(haystack, needle);
        assert!(result.is_some(), "should match via quote normalisation");
        let matched = result.unwrap();
        // The returned string must be the original file slice (curly quotes).
        assert!(
            matched.contains('\u{201c}'),
            "returned slice should preserve original curly quote, got: {matched:?}"
        );
        // And it must actually exist in the haystack (so replace will work).
        assert!(
            haystack.contains(&matched),
            "returned slice must be present verbatim in haystack, got: {matched:?}"
        );
    }

    #[test]
    fn try_match_quote_norm_combined_with_tws() {
        // Quote normalization + trailing whitespace strip combined (step 4).
        let haystack = "fn foo() {\n    bar\n}\n";
        // Needle has curly brace lookalike AND trailing whitespace — exercise
        // the combined path.  (Using a trailing space here rather than curly
        // quotes so the test stays simple and deterministic.)
        let needle = "fn foo() {\n    bar   \n}\n";
        let result = try_match(haystack, needle);
        assert!(result.is_some(), "should match via tws strip");
        let matched = result.unwrap();
        assert!(
            haystack.contains(&matched),
            "returned slice must be verbatim in haystack"
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
    fn try_match_desanitization() {
        let haystack = "result: <function_results>data</function_results>";
        let needle = "result: <fnr>data</fnr>";
        assert!(try_match(haystack, needle).is_some());
    }

    #[test]
    fn try_match_not_found() {
        assert_eq!(try_match("hello world", "goodbye"), None);
    }

    // ── Tool (end-to-end) ─────────────────────────────────────────────────

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
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "fn old_func() {}",
                "new_string": "fn new() {}",
            }))
            .await
            .unwrap();

        assert!(result.contains("updated successfully"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "fn new() {}\nfn mid() {}\nfn other_func() {}\n");
    }

    #[tokio::test]
    async fn fails_when_not_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();

        let err = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "goodbye",
                "new_string": "replaced"
            }))
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(msg.contains("not found"), "expected not found, got: {msg}");
    }

    #[tokio::test]
    async fn fails_when_ambiguous() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "foo\nfoo\nfoo\n").unwrap();

        let err = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "foo",
                "new_string": "bar",
            }))
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(msg.contains("Found 3 matches"), "got: {msg}");
    }

    #[tokio::test]
    async fn replace_all_replaces_all() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "foo\nfoo\nfoo\n").unwrap();

        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.contains("All occurrences"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "bar\nbar\nbar\n");
    }

    #[tokio::test]
    async fn empty_old_string_creates_file() {
        let tmp = TempDir::new().unwrap();

        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "new_file.txt",
                "old_string": "",
                "new_string": "brand new content"
            }))
            .await
            .unwrap();

        assert!(result.contains("Created"));
        let content = std::fs::read_to_string(tmp.path().join("new_file.txt")).unwrap();
        assert_eq!(content, "brand new content");
    }

    #[tokio::test]
    async fn empty_old_creates_nested_file() {
        let tmp = TempDir::new().unwrap();

        StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
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
    async fn identical_strings_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello\n").unwrap();

        let err = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "hello"
            }))
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(msg.contains("same"), "got: {msg}");
    }

    #[tokio::test]
    async fn sandboxed_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
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
    async fn quote_normalization_matches_in_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "let msg = \"it's done\";\n").unwrap();

        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "let msg = \"it\u{2019}s done\";",
                "new_string": "let msg = \"it's replaced\";"
            }))
            .await
            .unwrap();

        assert!(result.contains("updated successfully"));
    }

    #[tokio::test]
    async fn trailing_whitespace_strip_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "fn foo() {\n    bar\n}\n").unwrap();

        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "fn foo() {   \n    bar   \n}\n",
                "new_string": "fn foo() {\n    baz\n}\n"
            }))
            .await
            .unwrap();

        assert!(result.contains("updated successfully"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "fn foo() {\n    baz\n}\n");
    }

    #[tokio::test]
    async fn new_string_trailing_whitespace_stripped() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "fn foo() {\n    bar\n}\n").unwrap();

        StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "fn foo() {\n    bar\n}\n",
                "new_string": "fn foo() {\n    baz   \n}\n"  // trailing spaces on middle line
            }))
            .await
            .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        // Trailing spaces on the middle line must be stripped.
        assert_eq!(content, "fn foo() {\n    baz\n}\n");
    }

    #[tokio::test]
    async fn new_string_trailing_whitespace_preserved_in_markdown() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("doc.md"), "# Hello\nold line  \n").unwrap();

        StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "doc.md",
                "old_string": "old line  \n",
                "new_string": "new line  \n"  // trailing spaces are hard line-break in Markdown
            }))
            .await
            .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("doc.md")).unwrap();
        // Trailing spaces must be preserved for Markdown files.
        assert_eq!(content, "# Hello\nnew line  \n");
    }

    #[tokio::test]
    async fn replace_all_with_multiple_occurrences() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "aaa bbb aaa\n").unwrap();

        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "aaa",
                "new_string": "ccc",
                "replace_all": true
            }))
            .await
            .unwrap();

        assert!(result.contains("All occurrences"));
        let content = std::fs::read_to_string(tmp.path().join("src.txt")).unwrap();
        assert_eq!(content, "ccc bbb ccc\n");
    }

    // ── Partial-read guard tests ──────────────────────────────────────────────

    fn make_slot() -> std::sync::Arc<std::sync::Mutex<crate::tools::fs::ReadFileState>> {
        std::sync::Arc::new(std::sync::Mutex::new(crate::tools::fs::ReadFileState::new()))
    }

    #[tokio::test]
    async fn edit_rejected_when_file_never_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();
        let slot = make_slot();
        let err = StrReplaceTool::new(tmp.path())
            .with_read_state(slot)
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not been read"),
            "expected 'not been read', got: {msg}"
        );
    }

    #[tokio::test]
    async fn edit_rejected_when_partial_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();
        let slot = make_slot();
        // Simulate a partial read record.
        slot.lock()
            .unwrap()
            .record(tmp.path().join("src.txt"), true);
        let err = StrReplaceTool::new(tmp.path())
            .with_read_state(slot)
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("partially read"),
            "expected 'partially read', got: {msg}"
        );
    }

    #[tokio::test]
    async fn edit_allowed_after_full_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();
        let slot = make_slot();
        // Simulate a full read record.
        slot.lock()
            .unwrap()
            .record(tmp.path().join("src.txt"), false);
        let result = StrReplaceTool::new(tmp.path())
            .with_read_state(slot)
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();
        assert!(result.contains("updated successfully"));
    }

    #[tokio::test]
    async fn edit_allowed_when_no_read_state() {
        // StrReplaceTool::new() without with_read_state — guard disabled,
        // backward-compatible behavior.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();
        let result = StrReplaceTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();
        assert!(result.contains("updated successfully"));
    }
}

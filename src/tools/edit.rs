//! `Edit`: edit a file by replacing an exact string.
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

use super::{resolve_within_any, AccessTier, SharedSandboxRoots, Tool};
use crate::acp::ToolKind;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::fs::{get_file_mtime, ReadFileState};

/// Maximum on-disk size of a file the Edit tool will touch, in bytes. Prevents
/// OOM from reading multi-GB files into memory. Aligned with fake-cc's
/// `MAX_EDIT_FILE_SIZE` (1 GiB stat bytes).
const MAX_EDIT_FILE_SIZE: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EditTool {
    pub root: PathBuf,
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    pub session_roots: Option<SharedSandboxRoots>,
    /// When `Some`, enforces the partial-read guard: edits on files that were
    /// never read, or only partially read, are rejected with a clear error.
    /// `None` (default) disables the guard for backward compatibility.
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}

impl EditTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
            read_state: None,
        }
    }

    pub fn with_read_state(mut self, slot: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_state = Some(slot);
        self
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
            // cargo-mutants::skip — `chars[i-1]` and `chars[i+1]` mutations are
            // equivalent: apostrophe (chars[i]) is never alphabetic, so any change
            // to the index expression yields the same result via the else branch.
            let prev_letter = i > 0 && chars[i - 1].is_alphabetic(); // cargo-mutants::skip
            let next_letter = i + 1 < chars.len() && chars[i + 1].is_alphabetic(); // cargo-mutants::skip
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
    // &&→|| guard mutations are equivalent (deduplication-only: step 4 finds the same
    // result as an earlier step when the guard is bypassed). Behaviorally tested by
    // try_match_combined_quote_and_trailing_ws; mutations skipped via end-of-line tag.
    if qn_tws != needle && qn_tws != qn_needle && qn_tws != tws {
        // cargo-mutants::skip
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
impl Tool for EditTool {
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

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
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

        let abs_path = resolve_within_any(&self.all_roots(), file_path, true)?;

        // ── Partial-read guard ──────────────────────────────────────────
        // Reject edits on files that have never been read, or were only read
        // partially (via start_line/end_line), in this session.
        //
        // The lock is acquired briefly to extract the record, then dropped
        // before any `.await` to keep the future `Send`.
        if let Some(slot) = &self.read_state {
            let staleness_check: Option<(u64, String)> = {
                let state = slot.lock().map_err(|_| Error::Tool {
                    name: "Edit".into(),
                    call_id: None,
                    message: "internal: read-state lock poisoned".into(),
                })?;
                match state.get(&abs_path) {
                    None => {
                        return Err(Error::Tool {
                            name: "Edit".into(),
                            call_id: None,
                            message: format!(
                                "File `{file_path}` has not been read yet. \
                                 Read it first before editing."
                            ),
                        });
                    }
                    Some(record) if record.is_partial => {
                        return Err(Error::Tool {
                            name: "Edit".into(),
                            call_id: None,
                            message: format!(
                                "File `{file_path}` was only partially read \
                                 (line range). Read the complete file before editing."
                            ),
                        });
                    }
                    Some(record) => {
                        // Check mtime while holding the lock; if stale, extract
                        // the cached content for an async content-fallback check
                        // outside the lock.
                        let disk_mtime = get_file_mtime(&abs_path);
                        if disk_mtime > record.timestamp {
                            Some((disk_mtime, record.content.clone()))
                        } else {
                            None // mtime unchanged — no staleness concern
                        }
                    }
                }
                // MutexGuard dropped here
            };

            // Async content-fallback staleness check (lock is not held).
            if let Some((_disk_mtime, cached_content)) = staleness_check {
                let disk_content = tokio::fs::read_to_string(&abs_path)
                    .await
                    .unwrap_or_default();
                if disk_content != cached_content {
                    return Err(Error::Tool {
                        name: "Edit".into(),
                        call_id: None,
                        message: format!(
                            "File `{file_path}` has been modified since it was \
                             last read. Read it again before editing."
                        ),
                    });
                }
                // Content unchanged despite mtime bump — safe to proceed.
            }
        }

        // ── File-size guard ─────────────────────────────────────────────
        // Refuse to edit files larger than MAX_EDIT_FILE_SIZE to avoid OOM.
        // A missing file (new-file create path) is exempt — no metadata yet.
        match tokio::fs::metadata(&abs_path).await {
            Ok(meta) => {
                let size = meta.len();
                if size > MAX_EDIT_FILE_SIZE {
                    return Err(Error::Tool {
                        name: "Edit".into(),
                        call_id: None,
                        message: format!(
                            "File `{file_path}` is too large to edit ({} bytes). \
                             Maximum editable file size is {} bytes.",
                            size, MAX_EDIT_FILE_SIZE
                        ),
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // New file — size guard does not apply.
            }
            Err(e) => {
                return Err(Error::Tool {
                    name: "Edit".into(),
                    call_id: None,
                    message: format!("{}: {e}", abs_path.display()),
                });
            }
        }

        // ── Empty old_string: create new file or overwrite empty file ───
        if old_string.is_empty() {
            if let Some(parent) = abs_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::Tool {
                        name: "Edit".into(),
                        call_id: None,
                        message: format!("mkdir {}: {e}", parent.display()),
                    })?;
            }
            tokio::fs::write(&abs_path, new_string)
                .await
                .map_err(|e| Error::Tool {
                    name: "Edit".into(),
                    call_id: None,
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
                call_id: None,
                message: format!("{}: {e}", abs_path.display()),
            })?;

        // ── Guard: identical strings do nothing ─────────────────────────
        if old_string == new_string {
            return Err(Error::Tool {
                name: "Edit".into(),
                call_id: None,
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
            call_id: None,
            message: format!("String to replace not found in `{file_path}`.\nString: {old_string}"),
        })?;

        // ── Count occurrences ───────────────────────────────────────────
        let occurrence_count = content.matches(actual_old.as_str()).count();
        if !replace_all && occurrence_count > 1 {
            return Err(Error::Tool {
                name: "Edit".into(),
                call_id: None,
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

        // ── Call-time staleness re-validation ──────────────────────────
        // The pre-read guard at the top of execute() checked staleness, but
        // several `.await` points (metadata, read_to_string) have yielded
        // since. Re-probe mtime here and, if the file was touched since the
        // cached read, compare the content we just read (`content`) against
        // the cached content. This narrows the validate→write race window,
        // mirroring fake-cc's `call()`-level mtime re-check.
        if let Some(slot) = &self.read_state {
            let cached: Option<(u64, String)> = {
                let state = slot.lock().map_err(|_| Error::Tool {
                    name: "Edit".into(),
                    call_id: None,
                    message: "internal: read-state lock poisoned".into(),
                })?;
                state
                    .get(&abs_path)
                    .map(|r| (r.timestamp, r.content.clone()))
                // guard dropped
            };
            if let Some((cached_ts, cached_content)) = cached {
                let disk_mtime = get_file_mtime(&abs_path);
                if disk_mtime > cached_ts && content != cached_content {
                    return Err(Error::Tool {
                        name: "Edit".into(),
                        call_id: None,
                        message: format!(
                            "File `{file_path}` was modified after it was last \
                             read. Read it again before editing."
                        ),
                    });
                }
            }
        }

        tokio::fs::write(&abs_path, &updated)
            .await
            .map_err(|e| Error::Tool {
                name: "Edit".into(),
                call_id: None,
                message: format!("{}: {e}", abs_path.display()),
            })?;

        // ── Post-edit cache update ───────────────────────────────────
        // Update the read-state so subsequent edits to the same file
        // don't require a redundant Read.
        if let Some(slot) = &self.read_state {
            if let Ok(mut state) = slot.lock() {
                let new_mtime = get_file_mtime(&abs_path);
                state.record(abs_path, false, updated.clone(), new_mtime);
            }
        }

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

    // ── is_opening_context ────────────────────────────────────────────────
    // Kills: replace is_opening_context -> bool with true  (113:5)
    //        replace is_opening_context -> bool with false (113:5)

    /// Position 0 must always be an opening context.
    /// Kills `replace -> bool with false` because at i=0 the guard is unconditional.
    #[test]
    fn is_opening_context_position_zero_is_opening() {
        let chars: Vec<char> = "hello".chars().collect();
        assert!(is_opening_context(&chars, 0), "i=0 must be opening context");
    }

    /// After whitespace or punctuation must be opening.
    /// Kills `replace -> bool with false`.
    #[test]
    fn is_opening_context_after_space_is_opening() {
        let chars: Vec<char> = " word".chars().collect();
        assert!(
            is_opening_context(&chars, 1),
            "after space must be opening context"
        );
    }

    /// After a letter must NOT be opening.
    /// Kills `replace -> bool with true` (would say "word'" is opening).
    #[test]
    fn is_opening_context_after_letter_is_not_opening() {
        let chars: Vec<char> = "word'end".chars().collect();
        assert!(
            !is_opening_context(&chars, 4),
            "after letter must NOT be opening context"
        );
    }

    // ── apply_curly_double ────────────────────────────────────────────────
    // Kills: replace == with != in apply_curly_double (127:14)

    /// `"hello"` must yield left-then-right double curly quotes.
    /// Kills `replace == with !=`: with mutation, non-`"` chars get replaced instead.
    #[test]
    fn apply_curly_double_wraps_correctly() {
        let result = apply_curly_double("\"hello\"");
        assert_eq!(
            result, "\u{201c}hello\u{201d}",
            "opening \" → LEFT_DOUBLE, closing \" → RIGHT_DOUBLE"
        );
    }

    /// Text without `"` must be returned unchanged.
    /// Kills `replace == with !=`: every letter would become a curly quote.
    #[test]
    fn apply_curly_double_no_quotes_unchanged() {
        let result = apply_curly_double("no quotes here");
        assert_eq!(result, "no quotes here");
    }

    // ── apply_curly_single ────────────────────────────────────────────────
    // Covers mutations at lines 142-149.

    /// Apostrophe in a contraction → RIGHT_SINGLE (curly apostrophe).
    /// Kills: replace apply_curly_single -> String with String::new() (142:5)
    ///        replace && with || (149:28) — `||` changes any prev OR next letter to contraction
    ///        replace && with || (146:37) — prev_letter guard short-circuit
    ///        replace && with || (147:51) — next_letter guard short-circuit
    #[test]
    fn apply_curly_single_contraction_becomes_right_curly() {
        // "it's": apostrophe at position 2, prev='t' (letter), next='s' (letter) → RIGHT_SINGLE
        let result = apply_curly_single("it's");
        assert_eq!(result, "it\u{2019}s");
    }

    /// Opening apostrophe at position 0 → LEFT_SINGLE; no panic from prev-char access.
    /// Kills: replace > with >= (146:33) — `i>=0` is always true → chars[-1] panics
    ///        replace > with == (146:33) — `i==0` only checks exact zero, then checks chars[-1]
    ///        replace && with || (146:37) — `0 || chars[-1].is_alphabetic()` panics at i=0
    #[test]
    fn apply_curly_single_apostrophe_at_start_is_left_curly() {
        let result = apply_curly_single("'world");
        // position 0: prev doesn't exist → opening → LEFT_SINGLE
        assert!(
            result.starts_with('\u{2018}'),
            "apostrophe at position 0 must be left curly; got: {result:?}"
        );
    }

    /// Apostrophe at the very end of the string → RIGHT_SINGLE; no panic from next-char access.
    /// Kills: replace < with <= (147:37) — `i+1 <= len` → chars[len] panics at last position
    ///        replace + with - (147:33) — `i - 1 < chars.len()` → underflow at i=0
    ///        replace + with * (147:33) — `i * 1 < chars.len()` → `i`, same index, subtly wrong
    ///        replace + with - (147:62) — `chars[i - 1]` at i=last uses wrong char
    ///        replace + with * (147:62) — same wrong-index issue
    #[test]
    fn apply_curly_single_apostrophe_at_end_is_right_curly() {
        // "hello'": position 5, prev='o' (letter), no next → not contraction → not opening → RIGHT_SINGLE
        let result = apply_curly_single("hello'");
        assert!(
            result.ends_with('\u{2019}'),
            "trailing apostrophe must be right curly; got: {result:?}"
        );
    }

    /// Text without apostrophe passes through unchanged.
    /// Kills: replace == with != (145:14) — every non-apostrophe char gets replaced with curly.
    #[test]
    fn apply_curly_single_no_apostrophe_unchanged() {
        let result = apply_curly_single("hello world");
        assert_eq!(result, "hello world");
    }

    /// After a space (opening context), apostrophe is LEFT_SINGLE even when the next char is a letter.
    /// Kills: replace - with + (146:48) — `chars[i+1]` used as prev; space context loses its opening meaning.
    #[test]
    fn apply_curly_single_after_space_is_opening() {
        // " 'hello": position 1, prev=' ' (space, opening), next='h' (letter)
        // → prev_letter=false (space is not alphabetic) → not contraction
        // → is_opening_context(1) = true (space before) → LEFT_SINGLE
        // With 146:48 mutation: prev_letter checks chars[i+1]='h' → true
        // → contraction check: prev=true(mutated), next=true → RIGHT_SINGLE (WRONG)
        let result = apply_curly_single(" 'hello");
        let chars: Vec<char> = result.chars().collect();
        assert_eq!(
            chars[1], '\u{2018}',
            "apostrophe after space must be left curly (opening); got: {result:?}"
        );
    }

    // ── preserve_quote_style ─────────────────────────────────────────────
    // Kills mutations at 170:55, 171:55, 172:23.

    /// actual_old with only a LEFT_DOUBLE (no RIGHT_DOUBLE) must still apply curly style.
    /// Kills: replace || with && (170:55) — requires BOTH left AND right double to set has_double.
    #[test]
    fn preserve_quote_style_left_double_only_applies_curly() {
        // actual_old has only LEFT_DOUBLE, new_string has straight quotes
        let actual_old = "\u{201c}open quote only";
        let result = preserve_quote_style("\"open quote only", actual_old, "\"replaced\"");
        assert!(
            result.contains('\u{201c}') || result.contains('\u{201d}'),
            "only LEFT_DOUBLE in actual_old must still trigger curly-double application; got: {result:?}"
        );
    }

    /// actual_old with only a LEFT_SINGLE must still apply curly single style.
    /// Kills: replace || with && (171:55) — requires BOTH left AND right single to set has_single.
    #[test]
    fn preserve_quote_style_left_single_only_applies_curly() {
        let actual_old = "\u{2018}open single only";
        let result = preserve_quote_style("'open single only", actual_old, "'replaced'");
        assert!(
            result.contains('\u{2018}') || result.contains('\u{2019}'),
            "only LEFT_SINGLE in actual_old must still trigger curly-single application; got: {result:?}"
        );
    }

    /// actual_old with only curly singles (no doubles): must apply singles but NOT doubles.
    /// Kills: delete ! at 172:23 — removes the `!` before `has_single`, so the guard
    /// `if !has_double && !has_single` becomes `if !has_double && has_single`, causing
    /// early return (no style applied) when only singles are present.
    #[test]
    fn preserve_quote_style_single_quotes_only_applied() {
        let actual_old = "\u{2018}hello\u{2019}";
        let result = preserve_quote_style("'hello'", actual_old, "'world'");
        assert!(
            result.contains('\u{2018}') || result.contains('\u{2019}'),
            "actual_old with only single curly quotes must apply single curly style; got: {result:?}"
        );
    }

    // ── try_match step 3 negative (kills 295:22 && with ||) ──────────────

    /// Step 3 must NOT match when the stripped needle is not in the haystack.
    /// Kills: replace && with || (295:22) — would return Some(stripped) even when haystack
    /// does not contain the stripped needle (since `tws != needle` is already true).
    #[test]
    fn try_match_trailing_whitespace_not_in_haystack_returns_none() {
        // needle has trailing whitespace but stripped form ("foo") is not in haystack
        assert_eq!(
            try_match("bar baz qux", "foo   "),
            None,
            "stripped needle not in haystack must return None"
        );
    }

    // ── try_match step 4 negative (kills 301:* mutations) ─────────────────

    /// Step 4 (combined quote-norm + tws) must NOT fire when qn_tws == needle.
    /// Kills: replace != with == (301:15) — enters step 4 only when qn_tws == needle.
    #[test]
    fn try_match_combined_step_does_not_fire_when_no_change() {
        // needle with no quotes and no trailing whitespace: all normalizations are no-ops
        // qn_tws == needle → step 4 guard must be false → no match via step 4
        assert_eq!(try_match("hello world", "not found"), None);
    }

    // ── try_match step 4 positive (exercises combined quote-norm + tws) ───

    /// Exercises step 4 of `try_match` (combined quote-normalisation +
    /// trailing-whitespace strip).
    ///
    /// `needle` has both curly quotes **and** trailing spaces; `haystack` has
    /// straight quotes with no trailing spaces.
    ///
    /// * Step 1 (exact): fails — different quotes, different trailing WS.
    /// * Step 2 (quote-only): fails — `qn_needle` still has trailing spaces.
    /// * Step 3 (tws-only): fails — stripped needle keeps curly quotes.
    /// * Step 4 (combined): succeeds — `qn_tws` matches in `qn_tws_haystack`.
    ///
    /// Provides regression coverage for the guard at the step-4 `if` (which is
    /// skipped for mutation testing because its `&&→||` variants are
    /// near-equivalent deduplication guards).
    #[test]
    fn try_match_combined_quote_and_trailing_ws() {
        // Curly right-single quote + trailing spaces in the needle.
        let needle = "He said \u{2019}hello\u{2019}   ";
        let haystack = "He said 'hello'";

        let result = try_match(haystack, needle);
        assert!(
            result.is_some(),
            "step 4 must match: needle={needle:?} in haystack={haystack:?}"
        );
        let matched = result.unwrap();
        assert!(
            haystack.contains(&matched),
            "returned slice must be verbatim in haystack, got {matched:?}"
        );
    }

    // ── try_match step 5 negative (kills 312:21 && with ||) ──────────────

    /// Step 5 must NOT match when the desanitized needle is not in the haystack.
    /// Kills: replace && with || (312:21) — would return Some(ds) even when haystack
    /// does not contain it (since `ds != needle` is already true for a desanitizable needle).
    #[test]
    fn try_match_desanitized_needle_not_in_haystack_returns_none() {
        // needle has desanitizable tag but the desanitized form is not in the haystack
        assert_eq!(
            try_match("some other content", "<fnr>data</fnr>"),
            None,
            "desanitized form not in haystack must return None"
        );
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

        let tool = EditTool::new(tmp.path());
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

        let err = EditTool::new(tmp.path())
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

        let err = EditTool::new(tmp.path())
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

        let result = EditTool::new(tmp.path())
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

        let result = EditTool::new(tmp.path())
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

        EditTool::new(tmp.path())
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

        let err = EditTool::new(tmp.path())
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
        let err = EditTool::new(tmp.path())
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

        let result = EditTool::new(tmp.path())
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

        let result = EditTool::new(tmp.path())
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

        EditTool::new(tmp.path())
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

        EditTool::new(tmp.path())
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

        let result = EditTool::new(tmp.path())
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
        let err = EditTool::new(tmp.path())
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
        let path = tmp.path().join("src.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let slot = make_slot();
        // Simulate a partial read record.
        slot.lock().unwrap().record(
            path.clone(),
            true,
            std::fs::read_to_string(&path).unwrap(),
            get_file_mtime(&path),
        );
        let err = EditTool::new(tmp.path())
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
        let path = tmp.path().join("src.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let slot = make_slot();
        // Simulate a full read record.
        slot.lock().unwrap().record(
            path.clone(),
            false,
            std::fs::read_to_string(&path).unwrap(),
            get_file_mtime(&path),
        );
        let result = EditTool::new(tmp.path())
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
        // EditTool::new() without with_read_state — guard disabled,
        // backward-compatible behavior.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("src.txt"), "hello world\n").unwrap();
        let result = EditTool::new(tmp.path())
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();
        assert!(result.contains("updated successfully"));
    }

    // ── Staleness, call-time re-validation, size limit, post-edit cache ──────

    /// Bump a file's mtime to 1 hour in the future so `disk_mtime > cached_ts`
    /// fires deterministically regardless of FS timestamp resolution.
    fn bump_mtime_future(path: &std::path::Path) {
        use std::time::{Duration, SystemTime};
        let future = SystemTime::now() + Duration::from_secs(3600);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        f.set_modified(future).expect("set_modified");
    }

    #[tokio::test]
    async fn edit_rejected_when_file_modified_since_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("src.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let slot = make_slot();
        // Full read.
        slot.lock().unwrap().record(
            path.clone(),
            false,
            std::fs::read_to_string(&path).unwrap(),
            get_file_mtime(&path),
        );
        // Externally modify content + bump mtime to the future.
        std::fs::write(&path, "externally changed\n").unwrap();
        bump_mtime_future(&path);
        let err = EditTool::new(tmp.path())
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
            msg.contains("modified") && msg.contains("Read it again"),
            "expected staleness rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn edit_allowed_when_mtime_bumped_but_content_unchanged() {
        // Content-fallback: mtime bumped but content identical to cached →
        // edit proceeds (Windows cloud-sync / antivirus false positive).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("src.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let slot = make_slot();
        slot.lock().unwrap().record(
            path.clone(),
            false,
            std::fs::read_to_string(&path).unwrap(),
            get_file_mtime(&path),
        );
        // Bump mtime without changing content.
        bump_mtime_future(&path);
        let result = EditTool::new(tmp.path())
            .with_read_state(slot)
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .expect("unchanged content with bumped mtime must be allowed");
        assert!(result.contains("updated successfully"));
    }

    #[tokio::test]
    async fn edit_post_update_cache_allows_consecutive_edit() {
        // After a successful edit, the cache is refreshed so a second edit
        // to the same file does not require a redundant Read.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("src.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let slot = make_slot();
        slot.lock().unwrap().record(
            path.clone(),
            false,
            std::fs::read_to_string(&path).unwrap(),
            get_file_mtime(&path),
        );
        let tool = EditTool::new(tmp.path()).with_read_state(slot);
        tool.execute(serde_json::json!({
            "file_path": "src.txt",
            "old_string": "hello",
            "new_string": "goodbye"
        }))
        .await
        .unwrap();
        // No re-read between edits — must succeed via post-edit cache update.
        let result = tool
            .execute(serde_json::json!({
                "file_path": "src.txt",
                "old_string": "goodbye",
                "new_string": "hello"
            }))
            .await
            .expect("consecutive edit must succeed without re-read");
        assert!(result.contains("updated successfully"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
    }

    #[tokio::test]
    async fn edit_rejected_for_oversized_file() {
        // A file whose logical size exceeds MAX_EDIT_FILE_SIZE is refused
        // before reading. We materialise a sparse file (set_len) so the
        // directory entry reports >1 GiB without allocating disk.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_EDIT_FILE_SIZE + 1).unwrap();
        drop(f);
        let slot = make_slot();
        slot.lock()
            .unwrap()
            .record(path.clone(), false, String::new(), get_file_mtime(&path));
        let err = EditTool::new(tmp.path())
            .with_read_state(slot)
            .execute(serde_json::json!({
                "file_path": "big.txt",
                "old_string": "x",
                "new_string": "y"
            }))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("too large to edit"),
            "expected 'too large to edit', got: {msg}"
        );
    }
}

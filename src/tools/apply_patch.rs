//! `apply_patch`: apply a structured multi-file patch atomically.
//!
//! The patch format is the **V4A** flavour popularised by Anthropic and used
//! by OpenAI's Codex CLI: no line numbers, context lines locate the change
//! in the file. This is materially more robust than unified diff for
//! LLM-authored edits — models reliably mis-count line numbers, but they
//! reliably preserve a few lines of context.
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/foo.rs
//! @@ optional anchor
//!  context line (unchanged, must match the file exactly)
//! -line to remove (must match the file exactly)
//! +line to add
//!  context line
//! *** Add File: new/file.rs
//! +line one
//! +line two
//! *** Delete File: doomed/file.rs
//! *** End Patch
//! ```
//!
//! Reliability strategy:
//!   - **Strict matching.** Context+remove lines must match the live file
//!     exactly. No fuzzy matching.
//!   - **Single-match.** If a hunk's pattern appears 0 or >1 times in the
//!     file, the patch is rejected (the model can add an `@@ anchor` for
//!     disambiguation or include more context).
//!   - **Atomic.** All hunks are applied in memory first; only after every
//!     file's final state is computed do we touch the disk. A failure in
//!     hunk N rolls the whole patch back; nothing on disk changes.
//!   - **Sandboxed.** Every path goes through `resolve_within`.
//!   - **Useful errors.** The tool returns structured diagnostics (which
//!     file, which hunk, what was being matched) so the model can self-correct.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const UPDATE: &str = "*** Update File: ";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const HUNK_SEP: &str = "@@";

// ---- Tool ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ApplyPatch {
    pub root: PathBuf,
}

impl ApplyPatch {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: concat!(
                "Apply a V4A-format patch atomically. Format:\n",
                "*** Begin Patch\n",
                "*** Update File: <path>\n",
                "[@@ optional_anchor_line]\n",
                " context_line\n",
                "-line_to_remove\n",
                "+line_to_add\n",
                " context_line\n",
                "*** Add File: <path>\n",
                "+line_one\n",
                "+line_two\n",
                "*** Delete File: <path>\n",
                "*** End Patch\n",
                "Context lines must match the file exactly. If a hunk's pattern ",
                "appears multiple times in the file, add an @@ anchor (some unique ",
                "line that appears earlier in the file) before the hunk."
            ).into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The V4A patch text"}
                },
                "required": ["patch"]
            }),
        }
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let input = args["patch"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "apply_patch".into(),
            message: "missing `patch`".into(),
        })?;
        let patch = parse_patch(input)
            .map_err(|e| Error::Tool { name: "apply_patch".into(), message: e })?;
        let writes = stage(&patch, &self.root).await
            .map_err(|e| Error::Tool { name: "apply_patch".into(), message: e })?;
        commit(writes).await
            .map_err(|e| Error::Tool { name: "apply_patch".into(), message: e })
    }
}

// ---- AST -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub anchor: Option<String>,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileOp {
    Update { path: String, hunks: Vec<Hunk> },
    Add { path: String, content: String },
    Delete { path: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Patch {
    pub ops: Vec<FileOp>,
}

// ---- Parser ----------------------------------------------------------------

pub(crate) fn parse_patch(input: &str) -> std::result::Result<Patch, String> {
    let mut lines = input.lines().peekable();

    // Skip leading blank lines.
    while matches!(lines.peek(), Some(l) if l.trim().is_empty()) {
        lines.next();
    }

    let begin = lines.next().ok_or("empty patch")?;
    if begin.trim() != BEGIN {
        return Err(format!("first line must be `{BEGIN}`, got `{begin}`"));
    }

    let mut patch = Patch::default();
    let mut current_file: Option<FileOp> = None;

    loop {
        let Some(line) = lines.next() else {
            return Err(format!("patch terminated without `{END}`"));
        };

        if line.trim() == END {
            if let Some(op) = current_file.take() {
                patch.ops.push(op);
            }
            break;
        }

        // File header? Flush whatever we were building and start a new op.
        if let Some(path) = line.strip_prefix(UPDATE) {
            if let Some(op) = current_file.take() {
                patch.ops.push(op);
            }
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(format!("`{UPDATE}` with empty path"));
            }
            current_file = Some(FileOp::Update { path, hunks: vec![Hunk { anchor: None, lines: vec![] }] });
            continue;
        }
        if let Some(path) = line.strip_prefix(ADD) {
            if let Some(op) = current_file.take() {
                patch.ops.push(op);
            }
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(format!("`{ADD}` with empty path"));
            }
            current_file = Some(FileOp::Add { path, content: String::new() });
            continue;
        }
        if let Some(path) = line.strip_prefix(DELETE) {
            if let Some(op) = current_file.take() {
                patch.ops.push(op);
            }
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(format!("`{DELETE}` with empty path"));
            }
            current_file = Some(FileOp::Delete { path });
            continue;
        }

        // Body of the current file.
        let op = current_file.as_mut().ok_or_else(|| {
            format!("body line `{line}` before any `*** Update/Add/Delete File:` header")
        })?;

        match op {
            FileOp::Update { hunks, .. } => {
                if let Some(rest) = line.strip_prefix(HUNK_SEP) {
                    // New hunk; first hunk is pre-allocated above, so we only push if the current is non-empty.
                    let anchor_str = rest.trim();
                    let anchor = if anchor_str.is_empty() { None } else { Some(anchor_str.to_string()) };
                    let last = hunks.last_mut().unwrap();
                    if last.lines.is_empty() {
                        last.anchor = anchor;
                    } else {
                        hunks.push(Hunk { anchor, lines: vec![] });
                    }
                    continue;
                }

                let parsed = if let Some(rest) = line.strip_prefix('-') {
                    HunkLine::Remove(rest.to_string())
                } else if let Some(rest) = line.strip_prefix('+') {
                    HunkLine::Add(rest.to_string())
                } else if let Some(rest) = line.strip_prefix(' ') {
                    HunkLine::Context(rest.to_string())
                } else if line.is_empty() {
                    // Unprefixed blank lines are treated as a blank context line.
                    // (Models sometimes emit `\n` instead of ` \n`.)
                    HunkLine::Context(String::new())
                } else {
                    return Err(format!("unexpected line in Update hunk: `{line}`"));
                };
                hunks.last_mut().unwrap().lines.push(parsed);
            }
            FileOp::Add { content, .. } => {
                let body_line = if let Some(rest) = line.strip_prefix('+') {
                    rest
                } else if line.is_empty() {
                    ""
                } else {
                    return Err(format!("Add File body lines must start with `+`, got `{line}`"));
                };
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(body_line);
            }
            FileOp::Delete { .. } => {
                // No body allowed.
                if !line.trim().is_empty() {
                    return Err(format!("Delete File body must be empty, got `{line}`"));
                }
            }
        }
    }

    // Validation: paths unique, hunks non-empty for Update.
    let mut seen: HashSet<&String> = HashSet::new();
    for op in &patch.ops {
        let path = match op {
            FileOp::Update { path, .. } | FileOp::Add { path, .. } | FileOp::Delete { path } => path,
        };
        if !seen.insert(path) {
            return Err(format!("path `{path}` appears in patch more than once"));
        }
        if let FileOp::Update { hunks, .. } = op {
            if hunks.iter().all(|h| h.lines.is_empty()) {
                return Err(format!("Update File `{path}` has no hunks"));
            }
            for (i, h) in hunks.iter().enumerate() {
                if h.lines.is_empty() {
                    return Err(format!("Update File `{path}` hunk {} is empty", i + 1));
                }
                if !h.lines.iter().any(|l| matches!(l, HunkLine::Add(_) | HunkLine::Remove(_))) {
                    return Err(format!("Update File `{path}` hunk {} has no +/- lines", i + 1));
                }
            }
        }
    }

    Ok(patch)
}

// ---- Applier ---------------------------------------------------------------

struct StagedWrite {
    abs_path: PathBuf,
    rel_path: String,
    kind: StagedKind,
}

enum StagedKind {
    WriteText(String),
    Delete,
}

async fn stage(patch: &Patch, root: &std::path::Path) -> std::result::Result<Vec<StagedWrite>, String> {
    let mut staged = Vec::new();
    for op in &patch.ops {
        let path = match op {
            FileOp::Update { path, .. } | FileOp::Add { path, .. } | FileOp::Delete { path } => path,
        };
        let abs = resolve_within(root, path).map_err(|e| e.to_string())?;

        match op {
            FileOp::Update { hunks, .. } => {
                let current = tokio::fs::read_to_string(&abs)
                    .await
                    .map_err(|e| format!("update `{path}`: {e}"))?;
                let updated = apply_hunks(&current, hunks)
                    .map_err(|e| format!("update `{path}`: {e}"))?;
                staged.push(StagedWrite {
                    abs_path: abs,
                    rel_path: path.clone(),
                    kind: StagedKind::WriteText(updated),
                });
            }
            FileOp::Add { content, .. } => {
                if abs.exists() {
                    return Err(format!("add `{path}`: file already exists"));
                }
                staged.push(StagedWrite {
                    abs_path: abs,
                    rel_path: path.clone(),
                    kind: StagedKind::WriteText(content.clone()),
                });
            }
            FileOp::Delete { .. } => {
                if !abs.exists() {
                    return Err(format!("delete `{path}`: file does not exist"));
                }
                staged.push(StagedWrite { abs_path: abs, rel_path: path.clone(), kind: StagedKind::Delete });
            }
        }
    }
    Ok(staged)
}

async fn commit(staged: Vec<StagedWrite>) -> std::result::Result<String, String> {
    let mut summary = Vec::new();
    for w in &staged {
        match &w.kind {
            StagedKind::WriteText(content) => {
                if let Some(parent) = w.abs_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("mkdir for `{}`: {e}", w.rel_path))?;
                }
                tokio::fs::write(&w.abs_path, content)
                    .await
                    .map_err(|e| format!("write `{}`: {e}", w.rel_path))?;
                summary.push(format!("wrote {} ({} bytes)", w.rel_path, content.len()));
            }
            StagedKind::Delete => {
                tokio::fs::remove_file(&w.abs_path)
                    .await
                    .map_err(|e| format!("delete `{}`: {e}", w.rel_path))?;
                summary.push(format!("deleted {}", w.rel_path));
            }
        }
    }
    Ok(format!("applied {} change(s):\n{}", staged.len(), summary.join("\n")))
}

fn apply_hunks(content: &str, hunks: &[Hunk]) -> std::result::Result<String, String> {
    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    for (idx, hunk) in hunks.iter().enumerate() {
        // Pattern: context + remove lines in order. Replacement: context + add.
        let pattern: Vec<&str> = hunk.lines.iter().filter_map(|l| match l {
            HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
            HunkLine::Add(_) => None,
        }).collect();

        let replacement: Vec<String> = hunk.lines.iter().filter_map(|l| match l {
            HunkLine::Context(s) | HunkLine::Add(s) => Some(s.clone()),
            HunkLine::Remove(_) => None,
        }).collect();

        if pattern.is_empty() {
            // Pure-add hunk with no anchor: prepend to file. Discouraged but allowed.
            // We only allow this if the file is currently empty AND there's no anchor.
            if !lines.is_empty() || hunk.anchor.is_some() {
                return Err(format!(
                    "hunk {} has no context/remove lines; need at least one for matching",
                    idx + 1
                ));
            }
            lines.extend(replacement);
            continue;
        }

        // Find all positions where pattern matches.
        let mut matches: Vec<usize> = Vec::new();
        if pattern.len() <= lines.len() {
            for start in 0..=(lines.len() - pattern.len()) {
                if lines[start..start + pattern.len()]
                    .iter()
                    .zip(pattern.iter())
                    .all(|(actual, expected)| actual == expected)
                {
                    matches.push(start);
                }
            }
        }

        // Anchor filtering: the anchor text must appear in some line at or
        // before the END of the matched region. Spec'd anchors sit *above*
        // the hunk, but models sometimes inline them into the first context
        // line — both should disambiguate equally well.
        if let Some(anchor) = &hunk.anchor {
            matches.retain(|&start| {
                let end = start + pattern.len();
                lines[..end].iter().any(|l| l.contains(anchor.as_str()))
            });
        }

        let pos = match matches.len() {
            0 => {
                return Err(format!(
                    "hunk {} pattern not found{}.\nFirst 3 pattern lines:\n{}",
                    idx + 1,
                    hunk.anchor.as_deref().map(|a| format!(" (anchor `{a}`)")).unwrap_or_default(),
                    pattern.iter().take(3).map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
                ));
            }
            1 => matches[0],
            n => {
                return Err(format!(
                    "hunk {} pattern matches {} locations; add an `@@ anchor` line above the hunk to disambiguate",
                    idx + 1, n
                ));
            }
        };

        let end = pos + pattern.len();
        lines.splice(pos..end, replacement);
    }

    let mut result = lines.join("\n");
    if trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn p(input: &str) -> Patch {
        parse_patch(input).expect("parse")
    }

    // -- parser ----------------------------------------------------------

    #[test]
    fn parse_update_single_hunk() {
        let patch = p("\
*** Begin Patch
*** Update File: a.txt
 keep
-old
+new
*** End Patch
");
        assert_eq!(patch.ops.len(), 1);
        match &patch.ops[0] {
            FileOp::Update { path, hunks } => {
                assert_eq!(path, "a.txt");
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].anchor, None);
                assert_eq!(hunks[0].lines, vec![
                    HunkLine::Context("keep".into()),
                    HunkLine::Remove("old".into()),
                    HunkLine::Add("new".into()),
                ]);
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn parse_multi_hunk_with_anchors() {
        let patch = p("\
*** Begin Patch
*** Update File: a.txt
@@ fn foo
 a
-b
+B
@@ fn bar
 c
-d
+D
*** End Patch
");
        match &patch.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks.len(), 2);
                assert_eq!(hunks[0].anchor.as_deref(), Some("fn foo"));
                assert_eq!(hunks[1].anchor.as_deref(), Some("fn bar"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_add_file_collects_plus_lines() {
        let patch = p("\
*** Begin Patch
*** Add File: new.txt
+hello
+world
*** End Patch
");
        match &patch.ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "new.txt");
                assert_eq!(content, "hello\nworld");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_delete_file() {
        let patch = p("\
*** Begin Patch
*** Delete File: doomed.txt
*** End Patch
");
        assert!(matches!(&patch.ops[0], FileOp::Delete { path, .. } if path == "doomed.txt"));
    }

    #[test]
    fn parse_rejects_missing_begin() {
        let e = parse_patch("hello\n*** End Patch\n").unwrap_err();
        assert!(e.contains("first line"));
    }

    #[test]
    fn parse_rejects_missing_end() {
        let e = parse_patch("*** Begin Patch\n*** Update File: a\n keep\n").unwrap_err();
        assert!(e.contains("terminated"));
    }

    #[test]
    fn parse_rejects_duplicate_path() {
        let e = parse_patch("\
*** Begin Patch
*** Add File: a.txt
+x
*** Delete File: a.txt
*** End Patch
").unwrap_err();
        assert!(e.contains("more than once"));
    }

    #[test]
    fn parse_rejects_body_without_header() {
        let e = parse_patch("*** Begin Patch\n+stray\n*** End Patch\n").unwrap_err();
        assert!(e.contains("before any"));
    }

    #[test]
    fn parse_rejects_hunk_without_changes() {
        let e = parse_patch("\
*** Begin Patch
*** Update File: a
 just
 context
*** End Patch
").unwrap_err();
        assert!(e.contains("no +/- lines"));
    }

    // -- applier (in-memory) --------------------------------------------

    #[test]
    fn applies_single_hunk_round_trip() {
        let original = "line1\nold\nline3\n";
        let patch = p("\
*** Begin Patch
*** Update File: f
 line1
-old
+new
 line3
*** End Patch
");
        let FileOp::Update { hunks, .. } = &patch.ops[0] else { panic!() };
        let updated = apply_hunks(original, hunks).unwrap();
        assert_eq!(updated, "line1\nnew\nline3\n");
    }

    #[test]
    fn applies_multi_hunk() {
        let original = "alpha\nbeta\ngamma\ndelta\n";
        let hunks = vec![
            Hunk { anchor: None, lines: vec![
                HunkLine::Context("alpha".into()),
                HunkLine::Remove("beta".into()),
                HunkLine::Add("BETA".into()),
            ]},
            Hunk { anchor: None, lines: vec![
                HunkLine::Context("gamma".into()),
                HunkLine::Remove("delta".into()),
                HunkLine::Add("DELTA".into()),
            ]},
        ];
        assert_eq!(apply_hunks(original, &hunks).unwrap(), "alpha\nBETA\ngamma\nDELTA\n");
    }

    #[test]
    fn errors_when_pattern_not_found() {
        let e = apply_hunks("hello\nworld\n", &[Hunk { anchor: None, lines: vec![
            HunkLine::Context("foo".into()),
            HunkLine::Remove("bar".into()),
            HunkLine::Add("baz".into()),
        ]}]).unwrap_err();
        assert!(e.contains("not found"));
    }

    #[test]
    fn errors_when_pattern_ambiguous() {
        let original = "x\ny\nx\ny\n";
        let e = apply_hunks(original, &[Hunk { anchor: None, lines: vec![
            HunkLine::Context("x".into()),
            HunkLine::Remove("y".into()),
            HunkLine::Add("Y".into()),
        ]}]).unwrap_err();
        assert!(e.contains("matches"));
    }

    #[test]
    fn anchor_disambiguates_repeated_context() {
        // Two regions, same shape, anchor picks the second.
        let original = "fn foo {\n  x\n  y\n}\nfn bar {\n  x\n  y\n}\n";
        let hunks = vec![Hunk {
            anchor: Some("fn bar".into()),
            lines: vec![
                HunkLine::Context("  x".into()),
                HunkLine::Remove("  y".into()),
                HunkLine::Add("  Y".into()),
            ],
        }];
        let out = apply_hunks(original, &hunks).unwrap();
        assert_eq!(out, "fn foo {\n  x\n  y\n}\nfn bar {\n  x\n  Y\n}\n");
    }

    #[test]
    fn preserves_no_trailing_newline() {
        let original = "a\nb";
        let hunks = vec![Hunk { anchor: None, lines: vec![
            HunkLine::Remove("a".into()),
            HunkLine::Add("A".into()),
            HunkLine::Context("b".into()),
        ]}];
        assert_eq!(apply_hunks(original, &hunks).unwrap(), "A\nb");
    }

    // -- end-to-end (tool) ---------------------------------------------

    #[tokio::test]
    async fn tool_updates_existing_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "before\n").unwrap();
        let tool = ApplyPatch::new(tmp.path());
        let result = tool.execute(json!({"patch": "\
*** Begin Patch
*** Update File: hello.txt
-before
+after
*** End Patch
"})).await.unwrap();
        assert!(result.contains("wrote hello.txt"));
        assert_eq!(std::fs::read_to_string(tmp.path().join("hello.txt")).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn tool_adds_new_file() {
        let tmp = TempDir::new().unwrap();
        let tool = ApplyPatch::new(tmp.path());
        tool.execute(json!({"patch": "\
*** Begin Patch
*** Add File: sub/new.txt
+hi
+there
*** End Patch
"})).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("sub/new.txt")).unwrap(), "hi\nthere");
    }

    #[tokio::test]
    async fn tool_deletes_existing_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("doomed.txt"), "x").unwrap();
        ApplyPatch::new(tmp.path()).execute(json!({"patch": "\
*** Begin Patch
*** Delete File: doomed.txt
*** End Patch
"})).await.unwrap();
        assert!(!tmp.path().join("doomed.txt").exists());
    }

    #[tokio::test]
    async fn tool_is_atomic_on_failure() {
        // Two-file patch: first hunk succeeds (would update a.txt), second fails.
        // After apply, neither file should be modified.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "original-a\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "original-b\n").unwrap();
        let err = ApplyPatch::new(tmp.path()).execute(json!({"patch": "\
*** Begin Patch
*** Update File: a.txt
-original-a
+changed-a
*** Update File: b.txt
-nonexistent-line
+changed-b
*** End Patch
"})).await.unwrap_err();
        assert!(matches!(err, Error::Tool { .. }));
        assert_eq!(std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "original-a\n");
        assert_eq!(std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "original-b\n");
    }

    #[tokio::test]
    async fn tool_rejects_add_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("exists.txt"), "x").unwrap();
        let err = ApplyPatch::new(tmp.path()).execute(json!({"patch": "\
*** Begin Patch
*** Add File: exists.txt
+content
*** End Patch
"})).await.unwrap_err();
        assert!(matches!(err, Error::Tool { .. }));
    }

    #[tokio::test]
    async fn tool_rejects_delete_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let err = ApplyPatch::new(tmp.path()).execute(json!({"patch": "\
*** Begin Patch
*** Delete File: ghost.txt
*** End Patch
"})).await.unwrap_err();
        assert!(matches!(err, Error::Tool { .. }));
    }

    #[tokio::test]
    async fn tool_rejects_sandbox_escape() {
        let tmp = TempDir::new().unwrap();
        let err = ApplyPatch::new(tmp.path()).execute(json!({"patch": "\
*** Begin Patch
*** Add File: ../escape.txt
+evil
*** End Patch
"})).await.unwrap_err();
        // Wrapped as a Tool error (resolve_within returned BadToolArgs internally,
        // but we re-wrapped it). Just check it failed.
        assert!(matches!(err, Error::Tool { .. }));
        assert!(!tmp.path().parent().unwrap().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn tool_round_trips_a_real_change() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "\
fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn sub(a: i32, b: i32) -> i32 {
    a - b
}
").unwrap();
        let tool = ApplyPatch::new(tmp.path());
        tool.execute(json!({"patch": "\
*** Begin Patch
*** Update File: lib.rs
@@ fn sub
 fn sub(a: i32, b: i32) -> i32 {
-    a - b
+    a.saturating_sub(b)
 }
*** End Patch
"})).await.unwrap();
        let got = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(got.contains("a.saturating_sub(b)"));
        assert!(got.contains("a + b"), "add() unchanged");
    }
}

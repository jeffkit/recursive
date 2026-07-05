//! Filesystem tools: `Read`, `Write`.
//!
//! All paths are sandboxed to a workspace root. Reads/writes outside the
//! root are rejected at the tool layer, so the model can't (accidentally
//! or otherwise) touch the rest of the disk.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{resolve_within_any, AccessTier, SharedSandboxRoots, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

// ---------------------------------------------------------------------------
// ReadFileState — shared between ReadFile and EditTool
// ---------------------------------------------------------------------------

/// Per-file read record written by `ReadFile` and consumed by `EditTool`.
#[derive(Debug, Clone)]
pub struct ReadRecord {
    /// True when the read used start_line/end_line and did NOT cover the whole
    /// file. A read with start_line=1 and end_line=total_lines is a full read.
    pub is_partial: bool,
}

/// Session-scoped state tracking which files have been read (and whether
/// those reads were partial). Injected via `Arc<Mutex<ReadFileState>>` into
/// both `ReadFile` and `EditTool`.
#[derive(Debug, Default, Clone)]
pub struct ReadFileState {
    records: HashMap<PathBuf, ReadRecord>,
}

impl ReadFileState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, path: PathBuf, is_partial: bool) {
        self.records.insert(path, ReadRecord { is_partial });
    }

    pub fn get(&self, path: &Path) -> Option<&ReadRecord> {
        self.records.get(path)
    }
}

// ---------------------------------------------------------------------------
// ReadFile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReadFile {
    pub root: PathBuf,
    /// Additional sandbox roots beyond the primary workspace, each tagged
    /// with an access tier. The primary `root` is always
    /// [`AccessTier::ReadWrite`] and is prepended to this list at resolution
    /// time so the agent can read files in declared extra directories.
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    /// Session-scoped, runtime-mutable roots (e.g. added via the TUI
    /// `/add-dir` command). Consulted on every call so newly added roots
    /// take effect immediately.
    pub session_roots: Option<SharedSandboxRoots>,
    pub max_bytes: usize,
    /// Optional shared state slot. When `Some`, every successful read is
    /// recorded so `EditTool` can enforce the partial-read guard.
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}

impl ReadFile {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
            max_bytes: 256 * 1024,
            read_state: None,
        }
    }

    pub fn with_read_state(mut self, slot: Arc<Mutex<ReadFileState>>) -> Self {
        self.read_state = Some(slot);
        self
    }

    /// Append additional allowed sandbox roots (e.g. from `[sandbox]
    /// extra_dirs`). The primary workspace root is always prepended at
    /// resolution time, so callers only need to pass the *extra* ones here.
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

    /// Convenience: attach the shared slot only when `Some`. Used by
    /// [`crate::tools::build_standard_tools_with_roots`] so headless/CLI
    /// builds can pass `None` without conditional chaining at every call site.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    /// All roots, primary first, as consumed by [`resolve_within_any`].
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

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description:
                "Read a UTF-8 text file under the workspace. Optionally return a line range."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root"},
                    "start_line": {"type": "integer", "description": "Optional 1-indexed inclusive start line. If end_line is set but not start_line, defaults to 1."},
                    "end_line": {"type": "integer", "description": "Optional 1-indexed inclusive end line. If start_line is set but not end_line, defaults to last line."}
                },
                "required": ["path"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Read".into(),
            message: "missing `path`".into(),
        })?;
        let abs = resolve_within_any(&self.all_roots(), path, false)?;
        let bytes = tokio::fs::read(&abs).await.map_err(|e| Error::Tool {
            name: "Read".into(),
            call_id: None,
            message: format!("{}: {e}", abs.display()),
        })?;
        if bytes.len() > self.max_bytes {
            return Err(Error::Tool {
                name: "Read".into(),
                call_id: None,
                message: format!(
                    "file too large: {} bytes (max {})",
                    bytes.len(),
                    self.max_bytes
                ),
            });
        }
        let content = String::from_utf8(bytes).map_err(|e| Error::Tool {
            name: "Read".into(),
            call_id: None,
            message: format!("not utf-8: {e}"),
        })?;

        // Parse optional line range parameters
        let start_line = args["start_line"].as_u64();
        let end_line = args["end_line"].as_u64();

        // If no range specified, this is a full read.
        if start_line.is_none() && end_line.is_none() {
            if let Some(slot) = &self.read_state {
                if let Ok(mut state) = slot.lock() {
                    state.record(abs.clone(), false);
                }
            }
            return Ok(content);
        }

        // Count total lines
        let total_lines = content.lines().count();
        if total_lines == 0 {
            // Empty file — record as full read and return as-is.
            if let Some(slot) = &self.read_state {
                if let Ok(mut state) = slot.lock() {
                    state.record(abs.clone(), false);
                }
            }
            return Ok(content);
        }

        // Validate and clamp line numbers (1-indexed)
        let start = match start_line {
            Some(0) => {
                return Err(Error::BadToolArgs {
                    name: "Read".to_string(),
                    message: "start_line must be >= 1 (1-indexed)".to_string(),
                });
            }
            Some(n) => n as usize,
            None => 1,
        };

        let end = match end_line {
            Some(0) => {
                return Err(Error::BadToolArgs {
                    name: "Read".to_string(),
                    message: "end_line must be >= 1 (1-indexed)".to_string(),
                });
            }
            Some(n) => n as usize,
            None => total_lines,
        };

        // Validate start <= end
        if start > end {
            return Err(Error::BadToolArgs {
                name: "Read".to_string(),
                message: format!("start_line ({}) must be <= end_line ({})", start, end),
            });
        }

        // Clamp to valid range
        let start = start.min(total_lines);
        let end = end.min(total_lines);

        // Check if start exceeds total lines
        if start_line.is_some() && start > total_lines {
            return Err(Error::BadToolArgs {
                name: "Read".to_string(),
                message: format!("start_line {} exceeds total lines {}", start, total_lines),
            });
        }

        // A range covering the entire file counts as a full read.
        let is_partial = !(start == 1 && end == total_lines);
        if let Some(slot) = &self.read_state {
            if let Ok(mut state) = slot.lock() {
                state.record(abs.clone(), is_partial);
            }
        }

        // Extract the requested slice (1-indexed, inclusive)
        let slice: String = content
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .collect::<Vec<_>>()
            .join("\n");

        Ok(format!(
            "# range: lines {}-{} of {}\n{}",
            start, end, total_lines, slice
        ))
    }
}

#[derive(Debug, Clone)]
pub struct WriteFile {
    pub root: PathBuf,
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    pub session_roots: Option<SharedSandboxRoots>,
}

impl WriteFile {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
        }
    }

    /// Append additional allowed sandbox roots. See [`ReadFile::with_extra_roots`].
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

    /// Convenience: attach the shared slot only when `Some`. See
    /// [`ReadFile::with_session_roots_opt`].
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

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "Write/overwrite a UTF-8 text file under the workspace. Parent directories are created.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root"},
                    "contents": {"type": "string", "description": "Full new contents of the file"}
                },
                "required": ["path", "contents"]
            }),
        }
    }

    fn side_effect_class(&self) -> crate::tools::ToolSideEffect {
        crate::tools::ToolSideEffect::Mutating
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "Write".into(),
            message: "missing `path`".into(),
        })?;
        let contents = args["contents"]
            .as_str()
            .ok_or_else(|| Error::BadToolArgs {
                name: "Write".into(),
                message: "missing `contents`".into(),
            })?;
        let abs = resolve_within_any(&self.all_roots(), path, true)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Tool {
                    name: "Write".into(),
                    call_id: None,
                    message: format!("mkdir {}: {e}", parent.display()),
                })?;
        }
        tokio::fs::write(&abs, contents)
            .await
            .map_err(|e| Error::Tool {
                name: "Write".into(),
                call_id: None,
                message: format!("{}: {e}", abs.display()),
            })?;
        Ok(format!("wrote {} bytes to {}", contents.len(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let w = WriteFile::new(tmp.path());
        let r = ReadFile::new(tmp.path());
        w.execute(json!({"path":"hello.txt","contents":"world"}))
            .await
            .unwrap();
        let got = r.execute(json!({"path":"hello.txt"})).await.unwrap();
        assert_eq!(got, "world");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        WriteFile::new(tmp.path())
            .execute(json!({"path":"a/b/c.txt","contents":"x"}))
            .await
            .unwrap();
        assert!(tmp.path().join("a/b/c.txt").exists());
    }

    #[tokio::test]
    async fn rejects_escape() {
        let tmp = TempDir::new().unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r
            .execute(json!({"path":"../etc/passwd"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }
    // Tests for line range support (goal-26)
    #[tokio::test]
    async fn read_file_with_line_range() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("test.txt"),
            "line1
line2
line3
line4
line5
",
        )
        .unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r
            .execute(json!({"path":"test.txt", "start_line": 2, "end_line": 3}))
            .await
            .unwrap();
        // Should include range header and the sliced content
        assert!(got.starts_with(
            "# range: lines 2-3 of 5
"
        ));
        assert!(got.contains("line2"));
        assert!(got.contains("line3"));
        assert!(!got.contains("line1"));
        assert!(!got.contains("line4"));
        assert!(!got.contains("line5"));
    }

    #[tokio::test]
    async fn read_file_without_range_returns_full() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("test.txt"),
            "line1
line2
line3",
        )
        .unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r.execute(json!({"path":"test.txt"})).await.unwrap();
        // Should NOT have range header when no range specified
        assert!(!got.starts_with("# range:"));
        assert_eq!(
            got,
            "line1
line2
line3"
        );
    }

    #[tokio::test]
    async fn read_file_invalid_range_start_greater_than_end() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("test.txt"),
            "line1
line2
line3
",
        )
        .unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r
            .execute(json!({"path":"test.txt", "start_line": 10, "end_line": 5}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
        let err_msg = format!("{:?}", err);
        assert!(err_msg.contains("start_line") && err_msg.contains("end_line"));
    }

    // ── ReadFileState tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn read_state_records_full_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "line1\nline2\nline3\n").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let r = ReadFile::new(tmp.path()).with_read_state(slot.clone());
        r.execute(json!({"path": "f.txt"})).await.unwrap();
        let state = slot.lock().unwrap();
        let rec = state
            .get(&tmp.path().join("f.txt"))
            .expect("should be recorded");
        assert!(!rec.is_partial, "full read must be is_partial=false");
    }

    #[tokio::test]
    async fn read_state_records_partial_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let r = ReadFile::new(tmp.path()).with_read_state(slot.clone());
        r.execute(json!({"path": "f.txt", "start_line": 2, "end_line": 5}))
            .await
            .unwrap();
        let state = slot.lock().unwrap();
        let rec = state
            .get(&tmp.path().join("f.txt"))
            .expect("should be recorded");
        assert!(rec.is_partial, "line-range read must be is_partial=true");
    }

    #[tokio::test]
    async fn read_state_full_range_not_partial() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let r = ReadFile::new(tmp.path()).with_read_state(slot.clone());
        // start=1 end=5 covers all 5 lines → full read
        r.execute(json!({"path": "f.txt", "start_line": 1, "end_line": 5}))
            .await
            .unwrap();
        let state = slot.lock().unwrap();
        let rec = state
            .get(&tmp.path().join("f.txt"))
            .expect("should be recorded");
        assert!(!rec.is_partial, "start=1 end=N must be is_partial=false");
    }

    // ── extra_roots (sandbox expansion) tests ───────────────────────────────

    #[tokio::test]
    async fn read_file_from_extra_root() {
        use crate::tools::AccessTier;
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        std::fs::write(extra.path().join("outside.txt"), "hello-from-extra").unwrap();
        let r = ReadFile::new(ws.path())
            .with_extra_roots(vec![(extra.path().to_path_buf(), AccessTier::ReadOnly)]);
        let abs = extra.path().join("outside.txt");
        let got = r
            .execute(json!({"path": abs.to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(got, "hello-from-extra");
    }

    #[tokio::test]
    async fn write_file_blocked_on_readonly_extra_root() {
        use crate::tools::AccessTier;
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let w = WriteFile::new(ws.path())
            .with_extra_roots(vec![(extra.path().to_path_buf(), AccessTier::ReadOnly)]);
        let abs = extra.path().join("new.txt");
        let err = w
            .execute(json!({"path": abs.to_string_lossy(), "contents": "x"}))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("read-only"),
            "write to read-only extra root must be rejected: {err}"
        );
    }

    #[tokio::test]
    async fn write_file_allowed_on_readwrite_extra_root() {
        use crate::tools::AccessTier;
        let ws = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let w = WriteFile::new(ws.path())
            .with_extra_roots(vec![(extra.path().to_path_buf(), AccessTier::ReadWrite)]);
        let abs = extra.path().join("new.txt");
        w.execute(json!({"path": abs.to_string_lossy(), "contents": "x"}))
            .await
            .unwrap();
        assert!(abs.exists());
    }

    // ── max_bytes boundary (kills 165:24 > with == and > with >=) ───────────

    /// Kills: `replace > with ==` (165:24) and `replace > with >=` (165:24).
    ///
    /// The guard is `bytes.len() > max_bytes` (strictly greater-than), so a
    /// file of exactly `max_bytes` bytes must succeed.
    /// `> with ==` mutation: fires at exact size → false positive rejection.
    /// `> with >=` mutation: fires at exact size → false positive rejection.
    #[tokio::test]
    async fn read_file_exactly_max_bytes_succeeds() {
        let tmp = TempDir::new().unwrap();
        let max = 256 * 1024; // ReadFile::new default
        let content = "x".repeat(max);
        std::fs::write(tmp.path().join("big.txt"), content.as_bytes()).unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r.execute(json!({"path": "big.txt"})).await.unwrap();
        assert_eq!(got.len(), max, "file at exactly max_bytes must succeed");
    }

    /// Complementary: one byte over max_bytes must fail.
    #[tokio::test]
    async fn read_file_one_over_max_bytes_fails() {
        let tmp = TempDir::new().unwrap();
        let content = "x".repeat(256 * 1024 + 1);
        std::fs::write(tmp.path().join("toobig.txt"), content.as_bytes()).unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r.execute(json!({"path": "toobig.txt"})).await.unwrap_err();
        assert!(
            matches!(err, Error::Tool { .. }),
            "one byte over max_bytes must return Tool error: {err:?}"
        );
    }

    // ── only start_line given (kills 187:33 && with ||) ──────────────────────

    /// Kills: `replace && with ||` (187:33).
    ///
    /// The guard is `start_line.is_none() && end_line.is_none()`.
    /// With `||`, providing only start_line triggers the early-return and
    /// returns the full file without a range header.
    /// This test verifies that supplying start_line alone produces a range
    /// response (end defaults to total_lines).
    #[tokio::test]
    async fn read_file_start_only_returns_range_from_that_line() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "FIRST\nSECOND\nTHIRD\nFOURTH\nFIFTH",
        )
        .unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r
            .execute(json!({"path": "f.txt", "start_line": 3}))
            .await
            .unwrap();
        assert!(
            got.starts_with("# range:"),
            "start_line-only read must produce a range header; got: {got}"
        );
        assert!(got.contains("THIRD"), "line 3 must be present");
        assert!(got.contains("FOURTH"), "line 4 must be present");
        assert!(got.contains("FIFTH"), "line 5 must be present");
        assert!(!got.contains("FIRST"), "line 1 must be absent");
        assert!(!got.contains("SECOND"), "line 2 must be absent");
    }

    // ── single-line range (kills 232:18 > with >=) ────────────────────────────

    /// Kills: `replace > with >=` (232:18).
    ///
    /// The guard is `start > end` (rejects only strictly-greater).
    /// With `>=`, start == end is rejected as invalid, breaking single-line reads.
    #[tokio::test]
    async fn read_file_single_line_range_succeeds() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r
            .execute(json!({"path": "f.txt", "start_line": 2, "end_line": 2}))
            .await
            .unwrap();
        assert!(got.contains("beta"), "line 2 must be present");
        assert!(!got.contains("alpha"), "line 1 must be absent");
        assert!(!got.contains("gamma"), "line 3 must be absent");
    }

    // ── reading the last line (kills 244:42 > with == and > with >=) ─────────

    /// Kills: `replace > with ==` (244:42) and `replace > with >=` (244:42).
    ///
    /// After clamping, start == total_lines when reading the last line.
    /// Both mutations turn the dead-code guard into a live false-positive that
    /// rejects reading exactly the last line.
    #[tokio::test]
    async fn read_file_last_line_succeeds() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "first\nsecond\nthird").unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r
            .execute(json!({"path": "f.txt", "start_line": 3, "end_line": 3}))
            .await
            .unwrap();
        assert!(got.contains("third"), "last line must be returned");
        assert!(!got.contains("first"), "line 1 must be absent");
        assert!(!got.contains("second"), "line 2 must be absent");
    }

    // ── is_partial when starting at line 1 but not reaching end (kills 252:39) ──

    /// Kills: `replace && with ||` (252:39).
    ///
    /// `is_partial = !(start == 1 && end == total_lines)`.
    /// With `||` mutation: `!(start == 1 || end == total_lines)`.
    /// Reading lines 1..N-1 (start=1 but end < total) is a partial read
    /// (is_partial=true).  With `||`: `!(true || false)` = false → wrong.
    #[tokio::test]
    async fn read_state_partial_from_line_one_not_reaching_end() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let r = ReadFile::new(tmp.path()).with_read_state(slot.clone());
        // start=1, end=3 of 5 lines → partial (doesn't reach line 5)
        r.execute(json!({"path": "f.txt", "start_line": 1, "end_line": 3}))
            .await
            .unwrap();
        let state = slot.lock().unwrap();
        let rec = state
            .get(&tmp.path().join("f.txt"))
            .expect("must be recorded");
        assert!(
            rec.is_partial,
            "lines 1-3 of 5 must be recorded as partial (is_partial=true)"
        );
    }

    // ── range content for wider spans (kills 263:23 - with /) ─────────────────

    /// Kills: `replace - with /` (263:23).
    ///
    /// `take(end - start + 1)` vs `take(end / start + 1)`.
    /// For start=2, end=3: take(2) in both cases → equivalent.
    /// For start=2, end=5: original take(4), mutant take(5/2+1)=take(3).
    /// The 4th line ("four") would be missing with the mutant.
    #[tokio::test]
    async fn read_file_range_includes_all_lines_through_end() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let r = ReadFile::new(tmp.path());
        let got = r
            .execute(json!({"path": "f.txt", "start_line": 2, "end_line": 5}))
            .await
            .unwrap();
        assert!(got.contains("two"), "line 2 must be present");
        assert!(got.contains("three"), "line 3 must be present");
        assert!(got.contains("four"), "line 4 must be present");
        assert!(got.contains("five"), "line 5 must be present");
        assert!(!got.contains("one"), "line 1 must be absent");
    }
}

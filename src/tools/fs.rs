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
use crate::acp::ToolKind;
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

// ---------------------------------------------------------------------------
// ReadFileState — shared between ReadFile, EditTool, and WriteFile
// ---------------------------------------------------------------------------

/// Maximum number of entries in the read-state cache. When the cache exceeds
/// this limit, the oldest entry (by insertion order) is evicted. Aligned with
/// fake-cc's `READ_FILE_STATE_CACHE_SIZE` (100).
const READ_FILE_STATE_MAX_ENTRIES: usize = 100;

/// Per-file read record written by `ReadFile` and consumed by `EditTool` /
/// `WriteFile`.
#[derive(Debug, Clone)]
pub struct ReadRecord {
    /// True when the read used start_line/end_line and did NOT cover the whole
    /// file. A read with start_line=1 and end_line=total_lines is a full read.
    pub is_partial: bool,
    /// Cached file content at read time (CRLF → LF normalised). Used for
    /// staleness-check content fallback (mtime changed but content identical).
    pub content: String,
    /// File modification timestamp (`mtime` epoch millis) at read time.
    /// Compared against the on-disk mtime before allowing edits.
    pub timestamp: u64,
}

/// Session-scoped state tracking which files have been read (and whether
/// those reads were partial). Injected via `Arc<Mutex<ReadFileState>>` into
/// `ReadFile`, `EditTool`, and `WriteFile`.
#[derive(Debug, Default, Clone)]
pub struct ReadFileState {
    records: HashMap<PathBuf, ReadRecord>,
    /// Insertion order for LRU-style eviction.
    insertion_order: Vec<PathBuf>,
}

impl ReadFileState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a read (or post-edit/post-write update). Evicts the oldest
    /// entry when the cache exceeds [`READ_FILE_STATE_MAX_ENTRIES`].
    pub fn record(&mut self, path: PathBuf, is_partial: bool, content: String, timestamp: u64) {
        // Remove old insertion-order entry if it exists (re-insert at tail).
        if self.records.contains_key(&path) {
            self.insertion_order.retain(|p| p != &path);
        }
        self.records.insert(
            path.clone(),
            ReadRecord {
                is_partial,
                content,
                timestamp,
            },
        );
        self.insertion_order.push(path);

        // Evict oldest entries when over capacity.
        while self.insertion_order.len() > READ_FILE_STATE_MAX_ENTRIES {
            if let Some(oldest) = self.insertion_order.first().cloned() {
                self.insertion_order.remove(0);
                self.records.remove(&oldest);
            }
        }
    }

    pub fn get(&self, path: &Path) -> Option<&ReadRecord> {
        self.records.get(path)
    }
}

// ---------------------------------------------------------------------------
// File mtime helper
// ---------------------------------------------------------------------------

/// Return the file's modification time as epoch milliseconds (floored).
/// Returns 0 if the file does not exist or the metadata cannot be read.
pub(crate) fn get_file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    fn kind(&self) -> ToolKind {
        ToolKind::Read
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

        // Capture mtime right after reading so staleness checks are accurate.
        let mtime = get_file_mtime(&abs);

        // Parse optional line range parameters
        let start_line = args["start_line"].as_u64();
        let end_line = args["end_line"].as_u64();

        // If no range specified, this is a full read.
        if start_line.is_none() && end_line.is_none() {
            if let Some(slot) = &self.read_state {
                if let Ok(mut state) = slot.lock() {
                    state.record(abs.clone(), false, content.clone(), mtime);
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
                    state.record(abs.clone(), false, content.clone(), mtime);
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
                state.record(abs.clone(), is_partial, content.clone(), mtime);
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
    /// Optional shared state slot. When `Some`, writing to an *existing* file
    /// requires a prior full `Read` and checks staleness (file not modified
    /// since last read). New files (non-existent) are exempt.
    pub read_state: Option<Arc<Mutex<ReadFileState>>>,
}

impl WriteFile {
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
            description: "Write/overwrite a UTF-8 text file under the workspace. \
                          Parent directories are created.\n\
                          If the file already exists, you MUST read it first via the \
                          `Read` tool. This tool will error if you did not read an \
                          existing file before writing to it."
                .into(),
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

    fn kind(&self) -> ToolKind {
        ToolKind::Write
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

        // ── Pre-read guard for existing files ──────────────────────────
        // New files (non-existent on disk) are exempt — no prior Read needed.
        //
        // The lock is acquired briefly to extract the record, then dropped
        // before any `.await` to keep the future `Send`.
        let file_exists = abs.exists();
        if file_exists {
            if let Some(slot) = &self.read_state {
                let staleness_check: Option<(u64, String)> = {
                    let state = slot.lock().map_err(|_| Error::Tool {
                        name: "Write".into(),
                        call_id: None,
                        message: "internal: read-state lock poisoned".into(),
                    })?;
                    match state.get(&abs) {
                        None => {
                            return Err(Error::Tool {
                                name: "Write".into(),
                                call_id: None,
                                message: format!(
                                    "File `{path}` has not been read yet. \
                                     Read it first before writing to it."
                                ),
                            });
                        }
                        Some(record) if record.is_partial => {
                            return Err(Error::Tool {
                                name: "Write".into(),
                                call_id: None,
                                message: format!(
                                    "File `{path}` was only partially read \
                                     (line range). Read the complete file before writing."
                                ),
                            });
                        }
                        Some(record) => {
                            let disk_mtime = get_file_mtime(&abs);
                            if disk_mtime > record.timestamp {
                                Some((disk_mtime, record.content.clone()))
                            } else {
                                None
                            }
                        }
                    }
                    // MutexGuard dropped here
                };

                // Async content-fallback staleness check (lock is not held).
                if let Some((_disk_mtime, cached_content)) = staleness_check {
                    let disk_content = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
                    if disk_content != cached_content {
                        return Err(Error::Tool {
                            name: "Write".into(),
                            call_id: None,
                            message: format!(
                                "File `{path}` has been modified since it was \
                                 last read. Read it again before writing."
                            ),
                        });
                    }
                    // Content unchanged despite mtime bump — safe to proceed.
                }
            }
        }

        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Tool {
                    name: "Write".into(),
                    call_id: None,
                    message: format!("mkdir {}: {e}", parent.display()),
                })?;
        }

        // ── Call-time staleness re-validation ──────────────────────────
        // The pre-read guard above checked staleness, but `create_dir_all`
        // has yielded since. Re-probe mtime and, if the file was touched
        // since the cached read, compare disk content against the cached
        // content. Mirrors fake-cc's `FileWriteTool.call()` mtime re-check.
        if file_exists {
            if let Some(slot) = &self.read_state {
                let cached: Option<(u64, String)> = {
                    let state = slot.lock().map_err(|_| Error::Tool {
                        name: "Write".into(),
                        call_id: None,
                        message: "internal: read-state lock poisoned".into(),
                    })?;
                    state.get(&abs).map(|r| (r.timestamp, r.content.clone()))
                    // guard dropped
                };
                if let Some((cached_ts, cached_content)) = cached {
                    let disk_mtime = get_file_mtime(&abs);
                    if disk_mtime > cached_ts {
                        let disk_content =
                            tokio::fs::read_to_string(&abs).await.unwrap_or_default();
                        if disk_content != cached_content {
                            return Err(Error::Tool {
                                name: "Write".into(),
                                call_id: None,
                                message: format!(
                                    "File `{path}` was modified after it was last \
                                     read. Read it again before writing."
                                ),
                            });
                        }
                    }
                }
            }
        }

        tokio::fs::write(&abs, contents)
            .await
            .map_err(|e| Error::Tool {
                name: "Write".into(),
                call_id: None,
                message: format!("{}: {e}", abs.display()),
            })?;

        // ── Post-write cache update ──────────────────────────────────
        // Allow subsequent edits/writes without a redundant Read.
        if let Some(slot) = &self.read_state {
            if let Ok(mut state) = slot.lock() {
                let new_mtime = get_file_mtime(&abs);
                state.record(abs, false, contents.to_string(), new_mtime);
            }
        }

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
    #[tokio::test]
    async fn read_file_start_line_zero_is_invalid() {
        // kills `Some(0) =>` guard removal in the start_line match
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one\ntwo\n").unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r
            .execute(json!({"path": "f.txt", "start_line": 0}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::BadToolArgs { .. }),
            "start_line=0 must return BadToolArgs"
        );
    }

    #[tokio::test]
    async fn read_file_end_line_zero_is_invalid() {
        // kills `Some(0) =>` guard removal in the end_line match
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one\ntwo\n").unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r
            .execute(json!({"path": "f.txt", "end_line": 0}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::BadToolArgs { .. }),
            "end_line=0 must return BadToolArgs"
        );
    }

    #[tokio::test]
    async fn read_file_start_line_beyond_total_is_invalid() {
        // kills `if start_line.is_some() && start > total_lines` guard removal
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one\ntwo\n").unwrap();
        let r = ReadFile::new(tmp.path());
        let err = r
            .execute(json!({"path": "f.txt", "start_line": 999}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::BadToolArgs { .. }),
            "start_line beyond file length must return BadToolArgs"
        );
    }

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

    // ── WriteFile pre-read guard (aligned with fake-cc FileWriteTool) ───────

    /// Bump a file's mtime to 1 hour in the future so a staleness check based on
    /// `disk_mtime > cached_ts` fires deterministically regardless of FS
    /// timestamp resolution.
    fn bump_mtime_future(path: &Path) {
        use std::time::{Duration, SystemTime};
        let future = SystemTime::now() + Duration::from_secs(3600);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        f.set_modified(future).expect("set_modified");
    }

    #[tokio::test]
    async fn write_new_file_allowed_without_prior_read() {
        // New files (non-existent) are exempt from the pre-read guard.
        let tmp = TempDir::new().unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let w = WriteFile::new(tmp.path()).with_read_state(slot);
        w.execute(json!({"path": "fresh.txt", "contents": "hi"}))
            .await
            .expect("new file write must succeed without prior Read");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("fresh.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn write_existing_file_rejected_without_prior_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "orig").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        let w = WriteFile::new(tmp.path()).with_read_state(slot);
        let err = w
            .execute(json!({"path": "e.txt", "contents": "new"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("has not been read yet"),
            "expected 'has not been read yet', got: {msg}"
        );
    }

    #[tokio::test]
    async fn write_existing_file_rejected_when_partial_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        // Partial read via line range.
        ReadFile::new(tmp.path())
            .with_read_state(slot.clone())
            .execute(json!({"path": "e.txt", "start_line": 1, "end_line": 2}))
            .await
            .unwrap();
        let err = WriteFile::new(tmp.path())
            .with_read_state(slot)
            .execute(json!({"path": "e.txt", "contents": "new"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("partially read"),
            "expected 'partially read', got: {msg}"
        );
    }

    #[tokio::test]
    async fn write_succeeds_after_full_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "orig").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        ReadFile::new(tmp.path())
            .with_read_state(slot.clone())
            .execute(json!({"path": "e.txt"}))
            .await
            .unwrap();
        WriteFile::new(tmp.path())
            .with_read_state(slot)
            .execute(json!({"path": "e.txt", "contents": "rewritten"}))
            .await
            .expect("full read must allow write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rewritten");
    }

    #[tokio::test]
    async fn write_rejected_when_file_modified_since_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "orig").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        ReadFile::new(tmp.path())
            .with_read_state(slot.clone())
            .execute(json!({"path": "e.txt"}))
            .await
            .unwrap();
        // Externally modify the file content AND bump mtime to the future.
        std::fs::write(&path, "externally changed").unwrap();
        bump_mtime_future(&path);
        let err = WriteFile::new(tmp.path())
            .with_read_state(slot)
            .execute(json!({"path": "e.txt", "contents": "new"}))
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("modified") && msg.contains("Read it again"),
            "expected staleness rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn write_post_update_cache_allows_consecutive_write() {
        // After a successful write, the cache is refreshed so a second write
        // to the same file does not require a redundant Read.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "orig").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        ReadFile::new(tmp.path())
            .with_read_state(slot.clone())
            .execute(json!({"path": "e.txt"}))
            .await
            .unwrap();
        let w = WriteFile::new(tmp.path()).with_read_state(slot);
        w.execute(json!({"path": "e.txt", "contents": "v1"}))
            .await
            .unwrap();
        // No re-read between writes — must succeed via post-write cache update.
        w.execute(json!({"path": "e.txt", "contents": "v2"}))
            .await
            .expect("consecutive write must succeed without re-read");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    }

    #[tokio::test]
    async fn write_stale_mtime_but_unchanged_content_allowed() {
        // Content-fallback: mtime bumped but content identical to cached →
        // the write is allowed (Windows cloud-sync / antivirus false positive).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("e.txt");
        std::fs::write(&path, "same").unwrap();
        let slot = Arc::new(Mutex::new(ReadFileState::new()));
        ReadFile::new(tmp.path())
            .with_read_state(slot.clone())
            .execute(json!({"path": "e.txt"}))
            .await
            .unwrap();
        // Bump mtime without changing content.
        bump_mtime_future(&path);
        WriteFile::new(tmp.path())
            .with_read_state(slot)
            .execute(json!({"path": "e.txt", "contents": "new"}))
            .await
            .expect("unchanged content with bumped mtime must be allowed");
    }
}

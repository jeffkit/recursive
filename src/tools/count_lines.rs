//! `count_lines` tool: returns the number of lines in a text file.
//!
//! All paths are sandboxed to a workspace root, same as `ReadFile`.
//! File contents are read through the [`ToolTransport`] (Goal 402), so the
//! tool counts lines inside whatever execution environment the registry is
//! bound to — not on the host.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use super::transport::{retryable_prefix, ToolTransport};
use super::{resolve_within_any, AccessTier, SharedSandboxRoots, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

// ---------------------------------------------------------------------------
// CountLines
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CountLines {
    pub root: PathBuf,
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    pub session_roots: Option<SharedSandboxRoots>,
    /// Execution environment this tool reads through. Defaults to
    /// `LocalTransport`; builders inject the registry's transport so the
    /// tool follows the session's environment binding (Goal 402).
    pub transport: Arc<dyn ToolTransport>,
}

impl CountLines {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_roots: Vec::new(),
            session_roots: None,
            transport: Arc::new(super::transport::LocalTransport),
        }
    }

    /// Run file reads through a specific execution environment.
    pub fn with_transport(mut self, transport: Arc<dyn ToolTransport>) -> Self {
        self.transport = transport;
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

#[async_trait]
impl Tool for CountLines {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "count_lines".into(),
            description: "Return the number of lines in a text file inside the workspace.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace root"
                    }
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
            name: "count_lines".into(),
            message: "missing `path`".into(),
        })?;
        let abs = resolve_within_any(&self.all_roots(), path, false)?;
        let bytes = self
            .transport
            .read_file(&abs)
            .await
            .map_err(|e| Error::Tool {
                name: "count_lines".into(),
                call_id: None,
                message: format!("{}{}: {e}", retryable_prefix(&e), abs.display()),
            })?;
        let content = String::from_utf8(bytes).map_err(|e| Error::Tool {
            name: "count_lines".into(),
            call_id: None,
            message: format!("{}: {e}", abs.display()),
        })?;
        let count = content.lines().count();
        Ok(count.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn count_lines_happy_path() {
        let tmp = TempDir::new().unwrap();
        let contents = "line1\nline2\nline3\n";
        std::fs::write(tmp.path().join("test.txt"), contents).unwrap();
        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert_eq!(result, "3");
    }

    #[tokio::test]
    async fn count_lines_rejects_escape() {
        let tmp = TempDir::new().unwrap();
        let tool = CountLines::new(tmp.path());
        let err = tool
            .execute(json!({"path": "../etc/passwd"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn count_lines_empty_file_returns_zero() {
        // kills mutations that replace `content.lines().count()` with a non-zero value
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "empty.txt"})).await.unwrap();
        assert_eq!(result, "0");
    }

    #[tokio::test]
    async fn count_lines_no_trailing_newline() {
        // kills mutations that change line counting semantics;
        // `str::lines()` treats "a\nb" as 2 lines (no trailing newline does not add an empty line)
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("no_nl.txt"), "a\nb").unwrap();
        let tool = CountLines::new(tmp.path());
        let result = tool.execute(json!({"path": "no_nl.txt"})).await.unwrap();
        assert_eq!(result, "2");
    }

    #[tokio::test]
    async fn count_lines_missing_path_argument_errors() {
        // kills `ok_or_else(|| Error::BadToolArgs)` removal mutation
        let tmp = TempDir::new().unwrap();
        let tool = CountLines::new(tmp.path());
        let res = tool.execute(json!({})).await;
        assert!(
            matches!(res, Err(Error::BadToolArgs { .. })),
            "missing 'path' must return BadToolArgs"
        );
    }

    #[tokio::test]
    async fn count_lines_nonexistent_file_errors() {
        // kills `map_err(|e| Error::Tool {...})` removal mutation
        let tmp = TempDir::new().unwrap();
        let tool = CountLines::new(tmp.path());
        let res = tool.execute(json!({"path": "nonexistent.txt"})).await;
        assert!(
            matches!(res, Err(Error::Tool { .. })),
            "nonexistent file must return Tool error"
        );
    }

    // ── Goal 402: count_lines goes through the transport ─────────────────────

    /// In-memory transport: the file only exists inside the "environment".
    #[derive(Debug, Default)]
    struct MemoryTransport {
        files: std::collections::HashMap<PathBuf, Vec<u8>>,
    }

    impl MemoryTransport {
        fn with_file(mut self, path: &Path, contents: &[u8]) -> Self {
            self.files.insert(path.to_path_buf(), contents.to_vec());
            self
        }
    }

    #[async_trait]
    impl super::super::transport::ToolTransport for MemoryTransport {
        async fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
        }
        async fn write_file(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> std::io::Result<Vec<super::super::transport::DirEntry>> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn exec_shell(
            &self,
            _command: &str,
            _cwd: &Path,
            _env: &[(String, String)],
            _timeout: std::time::Duration,
            _max_output_bytes: usize,
        ) -> std::io::Result<super::super::transport::ExecResult> {
            Err(std::io::Error::other("unsupported"))
        }
    }

    /// A transport whose reads fail with a transient (timeout) error.
    #[derive(Debug)]
    struct TimedOutTransport;

    #[async_trait]
    impl super::super::transport::ToolTransport for TimedOutTransport {
        async fn read_file(&self, _path: &Path) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "environment unreachable",
            ))
        }
        async fn write_file(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> std::io::Result<Vec<super::super::transport::DirEntry>> {
            Err(std::io::Error::other("unsupported"))
        }
        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn exec_shell(
            &self,
            _command: &str,
            _cwd: &Path,
            _env: &[(String, String)],
            _timeout: std::time::Duration,
            _max_output_bytes: usize,
        ) -> std::io::Result<super::super::transport::ExecResult> {
            Err(std::io::Error::other("unsupported"))
        }
    }

    use std::path::Path;
    use std::sync::Arc;

    #[tokio::test]
    async fn count_lines_reads_through_transport_not_host_fs() {
        // The host tmp dir stays EMPTY — the file lives only in the transport.
        let host = TempDir::new().unwrap();
        let tool = CountLines::new(host.path()).with_transport(Arc::new(
            MemoryTransport::default()
                .with_file(&host.path().join("test.txt"), b"line1\nline2\nline3\n"),
        ));

        let out = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert_eq!(out, "3");
        assert!(
            !host.path().join("test.txt").exists(),
            "file must be read via transport — host fs must stay untouched"
        );
    }

    #[tokio::test]
    async fn count_lines_retryable_transport_failure_is_annotated() {
        let host = TempDir::new().unwrap();
        let tool = CountLines::new(host.path()).with_transport(Arc::new(TimedOutTransport));
        let err = tool.execute(json!({"path": "test.txt"})).await.unwrap_err();
        let msg = match err {
            Error::Tool { message, .. } => message,
            other => panic!("expected Tool error, got {other:?}"),
        };
        assert!(
            msg.starts_with("retryable: "),
            "timeout-classified transport failure must be marked retryable, got: {msg}"
        );
    }
}

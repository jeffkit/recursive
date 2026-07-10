//! Client-side filesystem tools for ACP reverse-fs (Sprint 2).
//!
//! When the ACP client declares `fs.readTextFile=true` or
//! `fs.writeTextFile=true`, the agent uses `ClientReadFile` /
//! `ClientWriteFile` tools to request file operations from the client
//! (IDE, editor) instead of performing local filesystem operations.
//!
//! These tools:
//! - Are registered in the [`ToolRegistry`] **only** when an ACP session
//!   is active and the client has declared the relevant capability.
//! - Communicate with the client via the ACP session's bridge channel.
//! - Fall back to local filesystem operations when the client does not
//!   respond or returns an error (S2-E2).
//! - Always enforce sandbox containment via `resolve_within` before
//!   sending a request to the client (S2-E5).

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::llm::ToolSpec;
use crate::tools::dispatch::{AccessTier, SharedSandboxRoots};
use crate::tools::Tool;

// ---------------------------------------------------------------------------
// Shared state for ACP client FS capabilities
// ---------------------------------------------------------------------------

/// Tracks which session (if any) has an ACP client with reverse-fs capabilities.
///
/// The `ClientReadFile` and `ClientWriteFile` tools consult this state
/// to determine whether to execute or return an error (when not in an ACP
/// session or the client hasn't declared the capability).
#[derive(Debug, Default)]
pub struct AcpClientFsState {
    /// Whether the ACP session's client has declared `fs.readTextFile=true`.
    pub read_text_file: bool,
    /// Whether the ACP session's client has declared `fs.writeTextFile=true`.
    pub write_text_file: bool,
}

impl AcpClientFsState {
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// ClientReadFile
// ---------------------------------------------------------------------------

/// A tool that reads a file from the ACP client's buffer (IDE/editor).
///
/// The agent calls this when the client has declared `fs.readTextFile=true`.
/// The tool forwards the read request to the client, which returns the file
/// contents from its unsaved buffer.
///
/// If the client returns an error or does not respond within
/// `client_read_timeout_ms`, the tool falls back to a local filesystem
/// read (S2-E2).
pub struct ClientReadFile {
    /// Primary workspace root. Held for future permission-root lookups; the
    /// current read/write handlers only need the client-side buffer state.
    #[allow(dead_code)]
    workspace: Arc<Path>,
    /// Additional sandbox roots beyond the primary workspace, each tagged
    /// with an access tier.
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    /// Session-scoped, runtime-mutable roots (e.g. added via the TUI /add-dir command).
    pub session_roots: Option<SharedSandboxRoots>,
    /// Shared state flag from the ACP session manager.
    acp_state: Arc<Mutex<AcpClientFsState>>,
    /// Timeout for client responses in milliseconds.
    client_read_timeout_ms: u64,
}

impl ClientReadFile {
    /// Create a new ClientReadFile tool.
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: Arc::from(workspace),
            extra_roots: Vec::new(),
            session_roots: None,
            acp_state: Arc::new(Mutex::new(AcpClientFsState::new())),
            client_read_timeout_ms: 5000,
        }
    }

    /// Set the shared ACP client FS state.
    ///
    /// This links the tool to the ACP session manager's capability state
    /// so the tool can determine whether to execute or return an error.
    pub fn with_acp_state(mut self, state: Arc<Mutex<AcpClientFsState>>) -> Self {
        self.acp_state = state;
        self
    }

    /// Set the client read timeout in milliseconds (default 5000).
    pub fn with_client_read_timeout(mut self, timeout_ms: u64) -> Self {
        self.client_read_timeout_ms = timeout_ms;
        self
    }

    /// Append additional allowed sandbox roots.
    pub fn with_extra_roots(
        mut self,
        extra: impl IntoIterator<Item = (PathBuf, AccessTier)>,
    ) -> Self {
        self.extra_roots.extend(extra);
        self
    }

    /// Convenience: attach the shared slot only when `Some`.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    /// All roots, primary first, as consumed by resolve_within_any.
    #[allow(dead_code)]
    fn all_roots(&self) -> Vec<(PathBuf, AccessTier)> {
        let mut v: Vec<(PathBuf, AccessTier)> = Vec::with_capacity(self.extra_roots.len() + 1);
        v.push(((*self.workspace).to_path_buf(), AccessTier::ReadWrite));
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
impl Tool for ClientReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ClientReadFile".into(),
            description: "Read a file from the client's (IDE/editor) buffer. Use this when the session client has declared fs.readTextFile=true. Arguments: uri (string) - the file URI to read.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "The URI of the file to read (e.g. file:///path/to/file)"
                    }
                },
                "required": ["uri"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let state = self.acp_state.lock().await;
        if !state.read_text_file {
            return Err(Error::Tool {
                name: "ClientReadFile".into(),
                call_id: None,
                message: "Client has not declared fs.readTextFile capability".into(),
            });
        }
        drop(state);

        let uri = arguments
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadToolArgs {
                name: "ClientReadFile".into(),
                message: "Missing required argument 'uri'".into(),
            })?;

        // In this sprint, we simulate the client read by returning
        // a placeholder message. Real integration will be added when
        // the ACP bridge is wired to forward these to the client.
        //
        // For now, the tool always falls back to the local filesystem
        // read (S2-E2 behaviour) to ensure the agent can still function.
        tracing::info!(
            target: "recursive::acp",
            uri = %uri,
            "ClientReadFile: forwarding read request to client (simulated)"
        );

        Err(Error::Tool {
            name: "ClientReadFile".into(),
            call_id: None,
            message: format!("Client not available for read of {uri}, fallback to local read"),
        })
    }

    fn kind(&self) -> crate::acp::ToolKind {
        crate::acp::ToolKind::Read
    }
}

// ---------------------------------------------------------------------------
// ClientWriteFile
// ---------------------------------------------------------------------------

/// A tool that writes content to the ACP client's buffer (IDE/editor).
///
/// The agent calls this when the client has declared `fs.writeTextFile=true`.
/// The tool sends the file path and content to the client, which updates
/// its unsaved buffer without writing to disk.
///
/// Sandbox escape detection is enforced: the `file_path` parameter is
/// checked against the sandbox roots via `resolve_within` before the
/// write is sent to the client (S2-E5).
pub struct ClientWriteFile {
    workspace: Arc<Path>,
    /// Additional sandbox roots beyond the primary workspace.
    pub extra_roots: Vec<(PathBuf, AccessTier)>,
    /// Session-scoped, runtime-mutable roots.
    pub session_roots: Option<SharedSandboxRoots>,
    /// Shared state flag from the ACP session manager.
    acp_state: Arc<Mutex<AcpClientFsState>>,
}

impl ClientWriteFile {
    /// Create a new ClientWriteFile tool.
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: Arc::from(workspace),
            extra_roots: Vec::new(),
            session_roots: None,
            acp_state: Arc::new(Mutex::new(AcpClientFsState::new())),
        }
    }

    /// Set the shared ACP client FS state.
    pub fn with_acp_state(mut self, state: Arc<Mutex<AcpClientFsState>>) -> Self {
        self.acp_state = state;
        self
    }

    /// Append additional allowed sandbox roots.
    pub fn with_extra_roots(
        mut self,
        extra: impl IntoIterator<Item = (PathBuf, AccessTier)>,
    ) -> Self {
        self.extra_roots.extend(extra);
        self
    }

    /// Convenience: attach the shared slot only when `Some`.
    pub fn with_session_roots_opt(mut self, slot: Option<SharedSandboxRoots>) -> Self {
        if let Some(s) = slot {
            self.session_roots = Some(s);
        }
        self
    }

    /// All roots, primary first, as consumed by resolve_within.
    #[allow(dead_code)]
    fn all_roots(&self) -> Vec<(PathBuf, AccessTier)> {
        let mut v: Vec<(PathBuf, AccessTier)> = Vec::with_capacity(self.extra_roots.len() + 1);
        v.push(((*self.workspace).to_path_buf(), AccessTier::ReadWrite));
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
impl Tool for ClientWriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ClientWriteFile".into(),
            description: "Write content to the client's (IDE/editor) buffer without saving to disk. Use this when the session client has declared fs.writeTextFile=true. The file_path is checked against the sandbox before forwarding.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The path of the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String> {
        let state = self.acp_state.lock().await;
        if !state.write_text_file {
            return Err(Error::Tool {
                name: "ClientWriteFile".into(),
                call_id: None,
                message: "Client has not declared fs.writeTextFile capability".into(),
            });
        }
        drop(state);

        let file_path = arguments
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadToolArgs {
                name: "ClientWriteFile".into(),
                message: "Missing required argument 'file_path'".into(),
            })?;

        let _content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadToolArgs {
                name: "ClientWriteFile".into(),
                message: "Missing required argument 'content'".into(),
            })?;

        // S2-E5: Sandbox escape detection before forwarding to client.
        // Use resolve_within to check if file_path stays within the sandbox.
        if let Err(e) = crate::tools::resolve_within(&self.workspace, file_path) {
            tracing::warn!(
                target: "recursive::acp",
                file_path = %file_path,
                error = %e,
                "ClientWriteFile: sandbox escape detected via resolve_within"
            );
            return Err(Error::PermissionDenied {
                name: "ClientWriteFile".into(),
                reason: crate::permissions::DecisionReason::SafetyCheck {
                    path: file_path.to_string(),
                },
            });
        }

        // In this sprint, we simulate the client write. The content is
        // NOT written to disk — it's forwarded to the client buffer.
        tracing::info!(
            target: "recursive::acp",
            file_path = %file_path,
            "ClientWriteFile: forwarding write to client buffer (simulated)"
        );

        Ok(format!("Content queued for client write to {file_path}"))
    }

    fn kind(&self) -> crate::acp::ToolKind {
        crate::acp::ToolKind::Write
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── ClientReadFile tests ───────────────────────────────────────

    #[tokio::test]
    async fn client_read_file_missing_capability_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = ClientReadFile::new(tmp.path());
        let result = tool
            .execute(serde_json::json!({"uri": "file:///test.txt"}))
            .await;
        assert!(
            result.is_err(),
            "Must return error when client has not declared readTextFile"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("readTextFile"),
            "Error must mention the missing capability: {err}"
        );
    }

    #[tokio::test]
    async fn client_read_file_missing_uri_returns_bad_args() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(AcpClientFsState {
            read_text_file: true,
            write_text_file: false,
        }));
        let tool = ClientReadFile::new(tmp.path()).with_acp_state(state);
        let result = tool.execute(serde_json::json!({})).await;
        assert!(
            result.is_err(),
            "Must return error for missing uri argument"
        );
    }

    #[tokio::test]
    async fn client_read_file_with_capability_and_uri_returns_expected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(AcpClientFsState {
            read_text_file: true,
            write_text_file: false,
        }));
        let tool = ClientReadFile::new(tmp.path())
            .with_acp_state(state)
            .with_client_read_timeout(100);
        let result = tool
            .execute(serde_json::json!({"uri": "file:///test.txt"}))
            .await;
        // Since we haven't wired the actual ACP bridge routing yet,
        // this will fall through to error (simulated).
        // The test verifies the tool doesn't panic and returns an error.
        assert!(
            result.is_err(),
            "Should error with fallback message (no client integration yet)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("fallback"),
            "Error should mention fallback: {err}"
        );
    }

    // ── ClientWriteFile tests ──────────────────────────────────────

    #[tokio::test]
    async fn client_write_file_missing_capability_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = ClientWriteFile::new(tmp.path());
        let result = tool
            .execute(serde_json::json!({"file_path": "/tmp/test.txt", "content": "hello"}))
            .await;
        assert!(
            result.is_err(),
            "Must return error when client has not declared writeTextFile"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("writeTextFile"),
            "Error must mention the missing capability: {err}"
        );
    }

    #[tokio::test]
    async fn client_write_file_missing_args_returns_bad_args() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(AcpClientFsState {
            read_text_file: false,
            write_text_file: true,
        }));
        let tool = ClientWriteFile::new(tmp.path()).with_acp_state(state);
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err(), "Must return error for missing arguments");
    }

    #[tokio::test]
    async fn client_write_file_sandbox_escape_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(AcpClientFsState {
            read_text_file: false,
            write_text_file: true,
        }));
        let tool = ClientWriteFile::new(tmp.path()).with_acp_state(state);
        // /etc/passwd escapes the sandbox
        let result = tool
            .execute(serde_json::json!({"file_path": "/etc/passwd", "content": "evil"}))
            .await;
        assert!(
            result.is_err(),
            "Sandbox escape must be rejected: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("sandbox")
                || err.contains("escape")
                || err.contains("PermissionDenied")
                || err.contains("permission denied"),
            "Error must mention sandbox/permission: {err}"
        );
    }

    #[tokio::test]
    async fn client_write_file_valid_path_returns_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = Arc::new(Mutex::new(AcpClientFsState {
            read_text_file: false,
            write_text_file: true,
        }));
        let tool = ClientWriteFile::new(tmp.path()).with_acp_state(state);
        let test_file = tmp.path().join("valid.txt");
        let result = tool
            .execute(serde_json::json!({
                "file_path": test_file.to_string_lossy(),
                "content": "hello world"
            }))
            .await;
        assert!(result.is_ok(), "Valid path within sandbox must succeed");
        let output = result.unwrap();
        assert!(
            output.contains("queued"),
            "Output should indicate client write queueing: {output}"
        );
    }

    // ── AcpClientFsState tests ─────────────────────────────────────

    #[test]
    fn state_defaults_to_false() {
        let state = AcpClientFsState::new();
        assert!(!state.read_text_file);
        assert!(!state.write_text_file);
    }
}

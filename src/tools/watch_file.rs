//! `watch_file` tool — register a file for mid-run event wakes.
//!
//! The TUI loop arbiter polls the registered file and re-invokes the agent
//! when new bytes are appended. This lets a supervising agent react to a
//! long-running command's structured events (one JSON object per line)
//! without burning a turn every tick — the agent only wakes when there is
//! something new to read.
//!
//! The watched file must live inside the workspace sandbox (resolved via
//! [`super::resolve_within`]). The watch state lives on the shared
//! [`BackgroundJobManager`] so the arbiter — which already holds that
//! manager — can poll it without a new shared slot.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::run_background::BackgroundJobManager;
use super::{resolve_within, Tool};
use crate::error::{Error, Result};
use crate::llm::ToolSpec;

/// The `watch_file` tool: register a file for event-driven wakes.
pub struct WatchFile {
    root: PathBuf,
    manager: Arc<Mutex<BackgroundJobManager>>,
}

impl WatchFile {
    pub fn new(root: impl Into<PathBuf>, manager: Arc<Mutex<BackgroundJobManager>>) -> Self {
        Self {
            root: root.into(),
            manager,
        }
    }
}

#[async_trait]
impl Tool for WatchFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "watch_file".into(),
            description: "Register a file (e.g. a command's events log) for \
                mid-run event wakes. The loop arbiter polls it and re-invokes \
                you only when new bytes are appended. Pair with /loop supervise: \
                after run_background, call watch_file on the command's events \
                file so you wake on each event instead of polling on a timer."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to watch (relative to workspace root; must stay inside the workspace)"
                    },
                    "from_end": {
                        "type": "boolean",
                        "description": "If true (default), only wake on bytes appended AFTER registration. If false, wake immediately with the whole file content.",
                        "default": true
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn is_deferred(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| Error::BadToolArgs {
            name: "watch_file".into(),
            message: "missing `path`".into(),
        })?;
        let resolved = resolve_within(&self.root, path).map_err(|e| Error::BadToolArgs {
            name: "watch_file".into(),
            message: format!("path: {e}"),
        })?;
        let from_end = args
            .get("from_end")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Snapshot the current file size (0 if not yet created) so from_end
        // starts past existing content.
        let size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);
        let offset = if from_end { size } else { 0 };

        let mut manager = self.manager.lock().await;
        manager.set_watch(resolved, offset);
        Ok(json!({
            "watching": path,
            "from_end": from_end,
            "initial_offset": offset,
            "message": format!(
                "Watching `{path}`. The loop arbiter will wake you when new bytes are appended (offset {offset})."
            )
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::run_background::WATCH_CHUNK_BYTES;
    use std::io::Write;
    use tempfile::TempDir;

    fn mgr() -> Arc<Mutex<BackgroundJobManager>> {
        Arc::new(Mutex::new(BackgroundJobManager::new()))
    }

    #[tokio::test]
    async fn watch_file_registers_watch_from_end() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        std::fs::write(&log, "existing\n").unwrap();
        let tool = WatchFile::new(tmp.path(), mgr());
        let out = tool.execute(json!({"path": "events.log"})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        // from_end=true → initial offset = existing file size, so existing
        // content does NOT wake the agent. "existing\n" is 9 bytes.
        assert_eq!(v["initial_offset"], 9);
        assert_eq!(v["from_end"], true);
    }

    #[tokio::test]
    async fn watch_file_from_end_false_starts_at_zero() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        std::fs::write(&log, "existing\n").unwrap();
        let tool = WatchFile::new(tmp.path(), mgr());
        let out = tool
            .execute(json!({"path": "events.log", "from_end": false}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["initial_offset"], 0);
    }

    #[tokio::test]
    async fn watch_file_rejects_path_outside_workspace() {
        let tmp = TempDir::new().unwrap();
        let tool = WatchFile::new(tmp.path(), mgr());
        let err = tool
            .execute(json!({"path": "../../etc/hosts"}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn watch_file_missing_path_errors() {
        let tmp = TempDir::new().unwrap();
        let tool = WatchFile::new(tmp.path(), mgr());
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, Error::BadToolArgs { .. }));
    }

    #[tokio::test]
    async fn poll_watch_returns_new_bytes_and_advances_offset() {
        let mut m = BackgroundJobManager::new();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        std::fs::write(&log, "old\n").unwrap();
        m.set_watch(log.clone(), 4); // past "old\n"
                                     // No new bytes yet.
        assert!(m.poll_watch().is_none());
        // Append new bytes.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "new1").unwrap();
        writeln!(f, "new2").unwrap();
        let first = m.poll_watch().expect("new bytes wake");
        assert!(first.contains("new1"));
        assert!(first.contains("new2"));
        // Offset advanced → no more new bytes.
        assert!(m.poll_watch().is_none());
    }

    #[tokio::test]
    async fn poll_watch_none_when_no_watch_set() {
        let mut m = BackgroundJobManager::new();
        assert!(m.poll_watch().is_none());
    }

    #[tokio::test]
    async fn poll_watch_handles_truncation_by_resetting() {
        let mut m = BackgroundJobManager::new();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        std::fs::write(&log, "0123456789").unwrap();
        m.set_watch(log.clone(), 10); // at EOF
                                      // Truncate + rewrite smaller (log rotation).
        std::fs::write(&log, "abc").unwrap();
        let got = m.poll_watch().expect("truncation should reset and read");
        assert!(got.contains("abc"));
    }

    #[tokio::test]
    async fn poll_watch_caps_chunk_size() {
        let mut m = BackgroundJobManager::new();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        // Write more than WATCH_CHUNK_BYTES.
        let big = "x".repeat(WATCH_CHUNK_BYTES + 1000);
        std::fs::write(&log, &big).unwrap();
        m.set_watch(log.clone(), 0);
        let first = m.poll_watch().expect("first chunk");
        assert_eq!(first.len(), WATCH_CHUNK_BYTES);
        // Second poll returns the remainder.
        let second = m.poll_watch().expect("second chunk");
        assert_eq!(second.len(), 1000);
        assert!(m.poll_watch().is_none());
    }

    #[tokio::test]
    async fn clear_watch_removes_watch() {
        let mut m = BackgroundJobManager::new();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("events.log");
        std::fs::write(&log, "x").unwrap();
        m.set_watch(log, 0);
        m.clear_watch();
        assert!(m.poll_watch().is_none());
    }

    #[tokio::test]
    async fn set_watch_replaces_prior_watch() {
        let mut m = BackgroundJobManager::new();
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.log");
        let b = tmp.path().join("b.log");
        std::fs::write(&a, "a-content").unwrap();
        std::fs::write(&b, "b-content").unwrap();
        m.set_watch(a.clone(), 0);
        m.set_watch(b.clone(), 0);
        // Only b is watched now.
        std::fs::write(&a, "a-content-more").unwrap();
        // a should not be polled (replaced). poll_watch reads b.
        let got = m.poll_watch().expect("b has content from offset 0");
        assert!(got.contains("b-content"));
    }
}

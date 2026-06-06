//! Local filesystem implementation of [`StorageBackend`].
//!
//! Stores transcripts as JSONL files and memory entries as plain text files,
//! both under `<workspace>/.recursive/` — identical layout to what Recursive
//! used before the trait abstraction existed.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;
use async_trait::async_trait;

/// Write `data` to `path` atomically using a sibling temp file + rename.
///
/// On most Unix filesystems `rename(2)` is atomic with respect to crashes,
/// so a reader will see either the old file or the complete new file — never
/// a partially-written one.
async fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let tmp_path = dir.join(format!(".tmp-{}-{}", filename, std::process::id()));
    tokio::fs::write(&tmp_path, data).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

/// [`StorageBackend`] backed by the local filesystem.
///
/// All data lives under `<workspace>/.recursive/`:
/// - Transcripts: `sessions/<session_id>.jsonl`
/// - Memory:      `memory/<key>`
pub struct LocalStorageBackend {
    workspace: PathBuf,
}

impl LocalStorageBackend {
    /// Create a new backend rooted at `workspace`.
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.workspace
            .join(".recursive")
            .join("sessions")
            .join(format!("{session_id}.jsonl"))
    }

    fn memory_path(&self, key: &str) -> PathBuf {
        self.workspace.join(".recursive").join("memory").join(key)
    }
}

#[async_trait]
impl StorageBackend for LocalStorageBackend {
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Message>> {
        let path = self.transcript_path(session_id);
        if !path.exists() {
            return Ok(vec![]);
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Error::Storage {
                message: format!("read transcript {path:?}: {e}"),
            })?;
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| Error::Storage {
                    message: format!("parse transcript line: {e}"),
                })
            })
            .collect()
    }

    async fn save_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let path = self.transcript_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Storage {
                    message: format!("create dir {parent:?}: {e}"),
                })?;
        }
        let mut lines = Vec::with_capacity(messages.len());
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| Error::Storage {
                message: format!("serialize message: {e}"),
            })?;
            lines.push(line);
        }
        atomic_write(&path, lines.join("\n").as_bytes())
            .await
            .map_err(|e| Error::Storage {
                message: format!("write transcript {path:?}: {e}"),
            })
    }

    async fn load_memory(&self, key: &str) -> Result<Option<String>> {
        let path = self.memory_path(key);
        if !path.exists() {
            return Ok(None);
        }
        tokio::fs::read_to_string(&path)
            .await
            .map(Some)
            .map_err(|e| Error::Storage {
                message: format!("read memory {key}: {e}"),
            })
    }

    async fn save_memory(&self, key: &str, value: &str) -> Result<()> {
        let path = self.memory_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Storage {
                    message: format!("create dir {parent:?}: {e}"),
                })?;
        }
        tokio::fs::write(&path, value)
            .await
            .map_err(|e| Error::Storage {
                message: format!("write memory {key}: {e}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Role;
    use tempfile::TempDir;

    fn backend() -> (LocalStorageBackend, TempDir) {
        let dir = TempDir::new().unwrap();
        let b = LocalStorageBackend::new(dir.path().to_path_buf());
        (b, dir)
    }

    fn make_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
            },
            Message {
                role: Role::Assistant,
                content: "world".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
            },
        ]
    }

    #[tokio::test]
    async fn save_and_load_transcript_roundtrip() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        b.save_transcript("sess1", &msgs).await.unwrap();
        let loaded = b.load_transcript("sess1").await.unwrap();
        assert_eq!(loaded, msgs);
    }

    #[tokio::test]
    async fn load_transcript_nonexistent_returns_empty() {
        let (b, _dir) = backend();
        let loaded = b.load_transcript("no-such-session").await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_memory_roundtrip() {
        let (b, _dir) = backend();
        b.save_memory("summary.md", "some memory text")
            .await
            .unwrap();
        let val = b.load_memory("summary.md").await.unwrap();
        assert_eq!(val.as_deref(), Some("some memory text"));
    }

    #[tokio::test]
    async fn load_memory_nonexistent_returns_none() {
        let (b, _dir) = backend();
        let val = b.load_memory("nonexistent").await.unwrap();
        assert!(val.is_none());
    }

    #[tokio::test]
    async fn save_transcript_creates_parent_dirs() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        // sessions directory does not exist yet
        b.save_transcript("deep-session", &msgs).await.unwrap();
        assert!(b.transcript_path("deep-session").exists());
    }
}

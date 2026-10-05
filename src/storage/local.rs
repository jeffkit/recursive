//! Local filesystem implementation of [`StorageBackend`].
//!
//! Stores transcripts as JSONL files and memory entries as plain text files,
//! both under `<workspace>/.recursive/` — identical layout to what Recursive
//! used before the trait abstraction existed.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;
use async_trait::async_trait;

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

/// Whether `path` exists, is non-empty, and does not end with `\n`.
///
/// Reads only the last byte (seek to end) so appending stays O(1) rather than
/// re-reading the whole transcript on the hot path. Any I/O error is treated
/// as "no separator needed" — the append itself will surface the failure.
async fn misses_trailing_newline(path: &std::path::Path) -> bool {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let Ok(len) = file.metadata().await.map(|m| m.len()) else {
        return false;
    };
    if len == 0 || file.seek(std::io::SeekFrom::End(-1)).await.is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await.is_ok() && last[0] != b'\n'
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
        // A pre-#92 writer emitted `lines.join("\n")` (no terminator); a torn
        // append can leave a truncated last line. Both leave the final line
        // unterminated — the truncated one is skipped so a crash mid-append
        // does not make the whole session unloadable. A malformed *terminated*
        // line is real corruption and still errors.
        let terminated = content.ends_with('\n');
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut messages = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            match serde_json::from_str(line) {
                Ok(m) => messages.push(m),
                Err(_) if !terminated && idx + 1 == lines.len() => break,
                Err(e) => {
                    return Err(Error::Storage {
                        message: format!("parse transcript line: {e}"),
                    })
                }
            }
        }
        Ok(messages)
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
        let mut body = String::new();
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| Error::Storage {
                message: format!("serialize message: {e}"),
            })?;
            body.push_str(&line);
            body.push('\n');
        }
        crate::atomic::atomic_write_async(&path, body.into_bytes())
            .await
            .map_err(|e| Error::Storage {
                message: format!("write transcript {path:?}: {e}"),
            })
    }

    async fn append_transcript(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let path = self.transcript_path(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::Storage {
                    message: format!("create dir {parent:?}: {e}"),
                })?;
        }
        let mut body = String::new();
        // A pre-#92 build wrote `lines.join("\n")` with no terminator
        // (and a torn append can leave a truncated last line). Without a
        // separator the first appended record merges onto that line and the
        // whole file becomes unparsable, so close the gap first.
        if misses_trailing_newline(&path).await {
            body.push('\n');
        }
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| Error::Storage {
                message: format!("serialize message: {e}"),
            })?;
            body.push_str(&line);
            body.push('\n');
        }
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| Error::Storage {
                message: format!("open transcript for append {path:?}: {e}"),
            })?;
        file.write_all(body.as_bytes())
            .await
            .map_err(|e| Error::Storage {
                message: format!("append transcript {path:?}: {e}"),
            })?;
        file.flush().await.map_err(|e| Error::Storage {
            message: format!("flush transcript {path:?}: {e}"),
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
        crate::atomic::atomic_write_async(&path, value.as_bytes().to_vec())
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
                is_compaction_summary: false,
            },
            Message {
                role: Role::Assistant,
                content: "world".into(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                is_compaction_summary: false,
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

    /// Issue #92: the incremental path must extend the existing JSONL without
    /// rewriting it, and `load_transcript` must see the appended messages in
    /// order.
    #[tokio::test]
    async fn append_transcript_extends_existing_transcript() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        b.save_transcript("sess1", &msgs[..1]).await.unwrap();
        b.append_transcript("sess1", &msgs[1..]).await.unwrap();

        assert_eq!(b.load_transcript("sess1").await.unwrap(), msgs);
    }

    #[tokio::test]
    async fn append_transcript_creates_missing_session() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        b.append_transcript("fresh", &msgs).await.unwrap();

        assert_eq!(b.load_transcript("fresh").await.unwrap(), msgs);
    }

    /// A pre-#92 writer produced `lines.join("\n")` — no trailing newline.
    /// Appending must insert the separator rather than merge two records into
    /// one unparsable line (which would 500 every cold load).
    #[tokio::test]
    async fn append_transcript_handles_legacy_file_without_trailing_newline() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        let legacy = format!(
            "{}\n{}",
            serde_json::to_string(&msgs[0]).unwrap(),
            serde_json::to_string(&msgs[1]).unwrap()
        );
        let path = b.transcript_path("legacy");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, legacy).await.unwrap();

        b.append_transcript("legacy", &msgs[..1]).await.unwrap();

        let loaded = b.load_transcript("legacy").await.unwrap();
        assert_eq!(loaded.len(), 3, "the legacy lines must survive the append");
        assert_eq!(loaded[0], msgs[0]);
        assert_eq!(loaded[1], msgs[1]);
        assert_eq!(loaded[2], msgs[0]);
    }

    /// A torn append (crash mid-write) leaves a truncated, unterminated final
    /// line; loading must keep the intact prefix instead of failing.
    #[tokio::test]
    async fn load_transcript_ignores_truncated_trailing_line() {
        let (b, _dir) = backend();
        let msgs = make_messages();
        b.save_transcript("torn", &msgs).await.unwrap();
        let path = b.transcript_path("torn");
        let mut content = tokio::fs::read_to_string(&path).await.unwrap();
        content.push_str("{\"role\":\"assist");
        tokio::fs::write(&path, content).await.unwrap();

        assert_eq!(b.load_transcript("torn").await.unwrap(), msgs);
    }

    /// The torn-line tolerance is narrow: a malformed line the writer did
    /// terminate is still corruption and must error.
    #[tokio::test]
    async fn load_transcript_rejects_corrupt_terminated_line() {
        let (b, _dir) = backend();
        let path = b.transcript_path("corrupt");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "not json\n").await.unwrap();

        assert!(b.load_transcript("corrupt").await.is_err());
    }

    #[tokio::test]
    async fn append_transcript_empty_slice_is_noop() {
        let (b, _dir) = backend();
        b.append_transcript("untouched", &[]).await.unwrap();

        assert!(
            !b.transcript_path("untouched").exists(),
            "an empty append must not materialize a transcript file"
        );
        assert!(b.load_transcript("untouched").await.unwrap().is_empty());
    }
}

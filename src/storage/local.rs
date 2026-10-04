//! Local filesystem implementation of [`StorageBackend`].
//!
//! Stores transcripts as JSONL files and memory entries as plain text files,
//! both under `<workspace>/.recursive/` — identical layout to what Recursive
//! used before the trait abstraction existed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};
use crate::message::Message;
use crate::storage::StorageBackend;
use async_trait::async_trait;

/// Mode of a persisted transcript / memory entry: owner read-write only.
///
/// Transcripts and memory entries are plaintext copies of the user's code and
/// conversation; under the usual umask (`022`) they would otherwise be
/// world-readable on shared hosts. It is applied at temp-file *creation* (see
/// [`crate::atomic::atomic_write_async_with_mode`]), not after the rename —
/// a post-rename chmod leaves the plaintext readable for the window in
/// between and permanently if the process dies inside it.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Extension of a persisted transcript file (`<session_id>.jsonl`).
const TRANSCRIPT_EXTENSION: &str = "jsonl";

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

    fn sessions_dir(&self) -> PathBuf {
        self.workspace.join(".recursive").join("sessions")
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.jsonl"))
    }

    fn memory_path(&self, key: &str) -> PathBuf {
        self.workspace.join(".recursive").join("memory").join(key)
    }

    fn tombstones_dir(&self) -> PathBuf {
        self.memory_path("session-deleted")
    }

    /// Delete every persisted transcript whose mtime is older than `cutoff`,
    /// except for the ids in `keep` (sessions still live in memory).
    ///
    /// A session's tombstone (`memory/session-deleted/<id>`) goes with its
    /// transcript — leaving it behind would grow the memory dir without
    /// bound and keep the id on disk after the transcript itself is gone. The
    /// same goes for the session's other keyed records
    /// ([`crate::storage::session_payload_keys`]): the metadata blob (title,
    /// custom system prompt) and the usage ledger are plaintext session data
    /// that retention must not outlive.
    ///
    /// Retention is keyed on the *file's* age, and a live session's snapshot is
    /// only rewritten when the session is evicted or closed — so a long-running
    /// session's file is arbitrarily old yet is the only copy a crash could
    /// recover (the in-memory transcript is lost with the process). `keep`
    /// excludes exactly those.
    ///
    /// Returns the number of persisted session records removed.
    pub async fn purge_sessions_older_than(
        &self,
        cutoff: SystemTime,
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let dir = self.sessions_dir();
        let mut removed = 0;
        match tokio::fs::read_dir(&dir).await {
            Ok(mut entries) => {
                while let Some(entry) = entries.next_entry().await.map_err(|e| Error::Storage {
                    message: format!("read sessions dir {dir:?}: {e}"),
                })? {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some(TRANSCRIPT_EXTENSION) {
                        continue;
                    }
                    let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if keep.contains(id) {
                        continue;
                    }
                    let mtime = entry.metadata().await.ok().and_then(|m| m.modified().ok());
                    if !is_expired(mtime, cutoff) {
                        continue;
                    }
                    remove_ignoring_missing(&path).await?;
                    // The tombstone and the per-session metadata/usage blobs
                    // name the same id and hold plaintext session data. Nothing
                    // enumerates `memory/`, so a sweep that left them behind
                    // would retain the session's title, custom system prompt and
                    // ledger past its retention window indefinitely.
                    for key in std::iter::once(crate::storage::deleted_marker_key(id))
                        .chain(crate::storage::session_payload_keys(id))
                    {
                        let _ = remove_ignoring_missing(&self.memory_path(&key)).await;
                    }
                    removed += 1;
                }
            }
            // A missing sessions dir means nothing to reap here — but a
            // tombstone may still be stranded, so fall through to the sweep.
            Err(e) if crate::storage::is_not_found(&e) => {}
            Err(e) => {
                return Err(Error::Storage {
                    message: format!("read sessions dir {dir:?}: {e}"),
                })
            }
        }
        removed += self.purge_orphan_tombstones(cutoff).await;
        Ok(removed)
    }

    /// Delete tombstones that are older than `cutoff` and whose transcript is
    /// already gone.
    ///
    /// The sweep above removes a tombstone together with its transcript; a
    /// tombstone whose transcript was already removed (a purge whose tombstone
    /// delete failed, a transcript removed out-of-band) would otherwise never
    /// expire — nothing enumerates `memory/` — so the deleted session id would
    /// outlive the transcript it names, which is exactly what a true delete
    /// must not leave behind.
    ///
    /// Best-effort: an unreadable tombstones dir means there is nothing to
    /// reclaim, and an individual failure is retried on the next sweep.
    async fn purge_orphan_tombstones(&self, cutoff: SystemTime) -> usize {
        let dir = self.tombstones_dir();
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            return 0;
        };
        let mut removed = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            let mtime = entry.metadata().await.ok().and_then(|m| m.modified().ok());
            if !is_expired(mtime, cutoff) || self.transcript_path(&id).exists() {
                continue;
            }
            if remove_ignoring_missing(&entry.path()).await.is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// Resolve a memory key under `<workspace>/.recursive/memory/`, rejecting any
/// component that would escape it.
///
/// Memory keys are namespaced (`session-deleted/<id>`) and reach the
/// filesystem straight from the HTTP layer, where axum percent-decodes `%2F`:
/// without this check the tombstone delete of a purge request for
/// `..%2F..%2Fimportant.txt` would remove a file outside the store.
fn contained_memory_path(workspace: &Path, key: &str) -> Result<PathBuf> {
    let mut path = workspace.join(".recursive").join("memory");
    for part in key.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Error::Storage {
                message: format!("unsafe memory key `{key}`"),
            });
        }
        path.push(part);
    }
    Ok(path)
}

/// Only a *known* mtime older than `cutoff` marks a transcript as expired.
///
/// An unreadable/unknowable mtime must never be a reason to delete user data —
/// retention is a cleanup, and the safe direction of an error is "keep".
fn is_expired(mtime: Option<SystemTime>, cutoff: SystemTime) -> bool {
    matches!(mtime, Some(t) if t < cutoff)
}

/// Remove `path`, treating "already gone" as success.
///
/// The caller holds no lock, so a concurrent purge (reaper tick vs. DELETE
/// purge) may have removed the file first; that is a success, not an error.
async fn remove_ignoring_missing(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if crate::storage::is_not_found(&e) => Ok(()),
        Err(e) => Err(Error::Storage {
            message: format!("delete {path:?}: {e}"),
        }),
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
        crate::atomic::atomic_write_async_with_mode(
            &path,
            lines.join("\n").into_bytes(),
            PRIVATE_FILE_MODE,
        )
        .await
        .map_err(|e| Error::Storage {
            message: format!("write transcript {path:?}: {e}"),
        })
    }

    async fn delete_transcript(&self, session_id: &str) -> Result<()> {
        // The id is interpolated into the file name and (for a purge) into git
        // refs, and it arrives straight from the HTTP path where axum
        // percent-decodes `%2F` — so it is validated before any deletion.
        crate::paths::validate_session_id(session_id)?;
        remove_ignoring_missing(&self.transcript_path(session_id)).await
    }

    async fn purge_expired_sessions(
        &self,
        max_age: Duration,
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        self.purge_sessions_older_than(cutoff, keep).await
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
        crate::atomic::atomic_write_async_with_mode(
            &path,
            value.as_bytes().to_vec(),
            PRIVATE_FILE_MODE,
        )
        .await
        .map_err(|e| Error::Storage {
            message: format!("write memory {key}: {e}"),
        })
    }

    async fn delete_memory(&self, key: &str) -> Result<()> {
        remove_ignoring_missing(&contained_memory_path(&self.workspace, key)?).await
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

    // ── Issue #102: true-delete + retention ───────────────────────────────

    #[tokio::test]
    async fn delete_transcript_removes_file_and_is_idempotent() {
        let (b, _dir) = backend();
        b.save_transcript("gone", &make_messages()).await.unwrap();
        assert!(b.transcript_path("gone").exists());

        b.delete_transcript("gone").await.unwrap();
        assert!(
            !b.transcript_path("gone").exists(),
            "delete_transcript must remove the persisted file"
        );
        assert!(b.load_transcript("gone").await.unwrap().is_empty());
        // Second delete (or delete of a never-persisted id) is a no-op.
        b.delete_transcript("gone").await.unwrap();
        b.delete_transcript("never-saved").await.unwrap();
    }

    #[tokio::test]
    async fn delete_memory_removes_entry_and_is_idempotent() {
        let (b, _dir) = backend();
        b.save_memory("session-deleted/sess-1", "1").await.unwrap();
        assert_eq!(
            b.load_memory("session-deleted/sess-1")
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );

        b.delete_memory("session-deleted/sess-1").await.unwrap();
        assert!(b
            .load_memory("session-deleted/sess-1")
            .await
            .unwrap()
            .is_none());
        b.delete_memory("session-deleted/sess-1").await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_and_memory_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (b, _dir) = backend();
        b.save_transcript("sess-perm", &make_messages())
            .await
            .unwrap();
        b.save_memory("user.md", "secret").await.unwrap();

        for path in [b.transcript_path("sess-perm"), b.memory_path("user.md")] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "{path:?} must be owner-only (0600), got {:o}",
                mode & 0o777
            );
        }
    }

    #[tokio::test]
    async fn purge_sessions_older_than_drops_old_keeps_fresh() {
        use std::time::{Duration, SystemTime};

        let (b, _dir) = backend();
        b.save_transcript("old", &make_messages()).await.unwrap();
        b.save_transcript("fresh", &make_messages()).await.unwrap();
        b.save_memory("session-deleted/old", "1").await.unwrap();
        b.save_memory("session-deleted/fresh", "1").await.unwrap();
        // Issue #98/#114: the metadata + usage records the session left in the
        // key/value space must expire with it.
        for id in ["old", "fresh"] {
            for key in crate::storage::session_payload_keys(id) {
                b.save_memory(&key, "{}").await.unwrap();
            }
        }

        // Backdate only the old session (its transcript + tombstone).
        let old_mtime = SystemTime::now() - Duration::from_secs(90 * 86_400);
        for path in [
            b.transcript_path("old"),
            b.memory_path("session-deleted/old"),
        ] {
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
                .unwrap();
        }

        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        assert_eq!(
            b.purge_sessions_older_than(cutoff, &HashSet::new())
                .await
                .unwrap(),
            1
        );

        assert!(
            !b.transcript_path("old").exists(),
            "expired transcript stays gone"
        );
        assert!(
            b.load_memory("session-deleted/old")
                .await
                .unwrap()
                .is_none(),
            "the expired session's tombstone must go with its transcript"
        );
        for key in crate::storage::session_payload_keys("old") {
            assert!(
                b.load_memory(&key).await.unwrap().is_none(),
                "the expired session's `{key}` record must go with its transcript"
            );
        }
        assert!(
            b.transcript_path("fresh").exists(),
            "fresh transcript survives"
        );
        assert_eq!(
            b.load_memory("session-deleted/fresh")
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
        for key in crate::storage::session_payload_keys("fresh") {
            assert!(
                b.load_memory(&key).await.unwrap().is_some(),
                "a fresh session keeps its `{key}` record"
            );
        }
    }

    #[tokio::test]
    async fn purge_expired_sessions_maps_window_onto_cutoff() {
        use std::time::{Duration, SystemTime};

        let (b, _dir) = backend();
        b.save_transcript("ancient", &make_messages())
            .await
            .unwrap();
        // Anything older than ~200 days is beyond a 30-day window.
        let old_mtime = SystemTime::now() - Duration::from_secs(200 * 86_400);
        let f = std::fs::File::options()
            .write(true)
            .open(b.transcript_path("ancient"))
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
            .unwrap();
        drop(f);

        assert_eq!(
            b.purge_expired_sessions(Duration::from_secs(30 * 86_400), &HashSet::new())
                .await
                .unwrap(),
            1
        );
        assert!(!b.transcript_path("ancient").exists());
    }

    /// Issue #102 review: retention keys off *file* age, and a live session's
    /// snapshot is only rewritten on eviction — so the sweep must not reap it.
    #[tokio::test]
    async fn purge_sessions_older_than_never_reaps_a_live_session() {
        use std::time::{Duration, SystemTime};

        let (b, _dir) = backend();
        b.save_transcript("running", &make_messages())
            .await
            .unwrap();
        b.save_transcript("closed", &make_messages()).await.unwrap();
        b.save_memory("session-deleted/running", "1").await.unwrap();
        b.save_memory("session-deleted/closed", "1").await.unwrap();

        // Both snapshots are ancient; only `running` is still in memory.
        let old_mtime = SystemTime::now() - Duration::from_secs(90 * 86_400);
        for path in [
            b.transcript_path("running"),
            b.memory_path("session-deleted/running"),
            b.transcript_path("closed"),
            b.memory_path("session-deleted/closed"),
        ] {
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
                .unwrap();
        }

        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let keep: HashSet<String> = ["running".to_string()].into_iter().collect();
        assert_eq!(b.purge_sessions_older_than(cutoff, &keep).await.unwrap(), 1);

        assert!(
            b.transcript_path("running").exists(),
            "a live session's snapshot is the only crash-recoverable copy"
        );
        assert_eq!(
            b.load_memory("session-deleted/running")
                .await
                .unwrap()
                .as_deref(),
            Some("1"),
            "the live session's tombstone must survive with its transcript"
        );
        assert!(!b.transcript_path("closed").exists());
    }

    /// Issue #102 review: a tombstone whose transcript is already gone is not
    /// reachable from the transcript sweep — it must expire on its own.
    #[tokio::test]
    async fn purge_sessions_older_than_reclaims_orphan_tombstones() {
        use std::time::{Duration, SystemTime};

        let (b, _dir) = backend();
        let old_mtime = SystemTime::now() - Duration::from_secs(90 * 86_400);
        // An orphan (no transcript) that is old, one that is fresh, and one
        // whose transcript still exists.
        b.save_memory("session-deleted/orphan-old", "1")
            .await
            .unwrap();
        b.save_memory("session-deleted/orphan-fresh", "1")
            .await
            .unwrap();
        b.save_transcript("paired", &make_messages()).await.unwrap();
        b.save_memory("session-deleted/paired", "1").await.unwrap();
        let backdate = |path: std::path::PathBuf| {
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
                .unwrap();
        };
        backdate(b.memory_path("session-deleted/orphan-old"));
        backdate(b.transcript_path("paired"));
        backdate(b.memory_path("session-deleted/paired"));

        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        // The paired transcript is reaped (with its tombstone); the orphan
        // tombstone is reclaimed by the same sweep.
        assert_eq!(
            b.purge_sessions_older_than(cutoff, &HashSet::new())
                .await
                .unwrap(),
            2
        );

        assert!(
            b.load_memory("session-deleted/orphan-old")
                .await
                .unwrap()
                .is_none(),
            "an orphan tombstone past the window must expire"
        );
        assert!(
            b.load_memory("session-deleted/orphan-fresh")
                .await
                .unwrap()
                .is_some(),
            "a fresh orphan tombstone is not expired yet"
        );
        assert!(
            b.load_memory("session-deleted/paired")
                .await
                .unwrap()
                .is_none(),
            "the paired tombstone goes with its transcript"
        );
    }

    /// Issue #102 review: the sweep must not touch a tombstone whose
    /// transcript is still there — the pair travels together.
    #[tokio::test]
    async fn orphan_sweep_keeps_a_tombstone_whose_transcript_exists() {
        use std::time::{Duration, SystemTime};

        let (b, _dir) = backend();
        b.save_transcript("live-file", &make_messages())
            .await
            .unwrap();
        b.save_memory("session-deleted/live-file", "1")
            .await
            .unwrap();
        let old_mtime = SystemTime::now() - Duration::from_secs(90 * 86_400);
        let f = std::fs::File::options()
            .write(true)
            .open(b.memory_path("session-deleted/live-file"))
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
            .unwrap();
        drop(f);

        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        assert_eq!(
            b.purge_sessions_older_than(cutoff, &HashSet::new())
                .await
                .unwrap(),
            0
        );
        assert!(
            b.load_memory("session-deleted/live-file")
                .await
                .unwrap()
                .is_some(),
            "a tombstone whose transcript survives must survive with it"
        );
    }

    #[test]
    fn is_expired_only_for_known_old_mtimes() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        assert!(is_expired(Some(old), now), "an older mtime is expired");
        assert!(!is_expired(Some(now), now), "an equal mtime is not expired");
        assert!(
            !is_expired(Some(now + Duration::from_secs(60)), now),
            "a newer mtime is not expired"
        );
        assert!(
            !is_expired(None, now),
            "an unreadable mtime must never expire user data"
        );
    }

    #[test]
    fn transcript_and_memory_paths_live_under_dot_recursive() {
        let dir = TempDir::new().unwrap();
        let b = LocalStorageBackend::new(dir.path().to_path_buf());
        assert_eq!(
            b.transcript_path("sess-1"),
            dir.path()
                .join(".recursive")
                .join("sessions")
                .join("sess-1.jsonl")
        );
        assert_eq!(
            b.memory_path("user.md"),
            dir.path().join(".recursive").join("memory").join("user.md")
        );
    }

    #[tokio::test]
    async fn purge_on_missing_sessions_dir_is_zero() {
        use std::time::{Duration, SystemTime};
        let (b, _dir) = backend();
        assert_eq!(
            b.purge_sessions_older_than(SystemTime::now(), &HashSet::new())
                .await
                .unwrap(),
            0
        );
        // The trait-facing entry point must be lenient too.
        assert_eq!(
            b.purge_expired_sessions(Duration::from_secs(86_400), &HashSet::new())
                .await
                .unwrap(),
            0
        );
    }

    /// "Missing" is the only read error the sweep may swallow. Anything else
    /// (here: a plain file where the sessions dir belongs → ENOTDIR) is a real
    /// failure and must reach the caller, not be reported as "nothing removed".
    #[tokio::test]
    async fn purge_surfaces_read_errors_other_than_missing() {
        use std::time::SystemTime;
        let (b, dir) = backend();
        let sessions = dir.path().join(".recursive").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::remove_dir(&sessions).unwrap();
        std::fs::write(&sessions, "not a directory").unwrap();

        assert!(
            b.purge_sessions_older_than(SystemTime::now(), &HashSet::new())
                .await
                .is_err(),
            "a non-NotFound read_dir failure must not be swallowed"
        );
    }

    /// Mirror of the above for the delete path: only "already gone" is success.
    #[tokio::test]
    async fn delete_transcript_surfaces_a_failed_removal() {
        let (b, dir) = backend();
        // A directory where the transcript file belongs: `remove_file` fails
        // with EPERM/EISDIR, neither of which is NotFound.
        std::fs::create_dir_all(dir.path().join(".recursive/sessions/stuck.jsonl")).unwrap();

        assert!(
            b.delete_transcript("stuck").await.is_err(),
            "a failed removal must not be reported as success"
        );
    }

    // ── Issue #102 review: the delete paths stay inside `.recursive/` ─────

    /// The id arrives from the HTTP path, where axum percent-decodes `%2F`,
    /// so `../..` must be rejected before it is interpolated into a file name.
    #[tokio::test]
    async fn delete_transcript_rejects_traversal_ids() {
        let (b, dir) = backend();
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, "must survive").unwrap();

        assert!(b.delete_transcript("../../outside").await.is_err());
        assert!(
            outside.exists(),
            "a traversal id must not delete a file outside the sessions dir"
        );

        // The two-segment form (what `..%2F..%2F` decodes to).
        assert!(b.delete_transcript("..").await.is_err());
        assert!(b.delete_transcript("a/b").await.is_err());
        assert!(b.delete_transcript("a\\b").await.is_err());
        assert!(b.delete_transcript(".hidden").await.is_err());
        assert!(b.delete_transcript("").await.is_err());
        assert!(outside.exists(), "no rejected id may have deleted anything");
        // A normal id still works.
        b.save_transcript("ok-1", &make_messages()).await.unwrap();
        b.delete_transcript("ok-1").await.unwrap();
        assert!(!b.transcript_path("ok-1").exists());
    }

    /// The tombstone key is namespaced (`session-deleted/<id>`), so it cannot
    /// be check with the session-id rule — it is contained instead.
    #[tokio::test]
    async fn delete_memory_rejects_keys_that_escape_the_memory_dir() {
        let (b, dir) = backend();
        let outside = dir.path().join("important.txt");
        std::fs::write(&outside, "must survive").unwrap();

        for key in [
            "../important.txt",
            "session-deleted/../../important.txt",
            "session-deleted/..",
            "session-deleted/",
            "/important.txt",
        ] {
            assert!(
                b.delete_memory(key).await.is_err(),
                "key `{key}` must be rejected"
            );
        }
        assert!(outside.exists(), "no key may escape the memory dir");

        // The real tombstone key still deletes.
        b.save_memory("session-deleted/sess-1", "1").await.unwrap();
        b.delete_memory("session-deleted/sess-1").await.unwrap();
        assert!(b
            .load_memory("session-deleted/sess-1")
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn contained_memory_path_keeps_namespaced_keys_inside() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            contained_memory_path(dir.path(), "session-deleted/sess-1").unwrap(),
            dir.path()
                .join(".recursive")
                .join("memory")
                .join("session-deleted")
                .join("sess-1")
        );
        assert_eq!(
            contained_memory_path(dir.path(), "user.md").unwrap(),
            dir.path().join(".recursive").join("memory").join("user.md")
        );
        assert!(contained_memory_path(dir.path(), "a/../b").is_err());
        assert!(contained_memory_path(dir.path(), "").is_err());
        assert!(contained_memory_path(dir.path(), "a//b").is_err());
    }
}

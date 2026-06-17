//! Session lifecycle operations: locking, truncation, and UUID chain recovery.
//!
//! Merges the former `session_lock.rs` (Goal 151 sentinel lock) with
//! `truncate_transcript_to_turn` and `read_last_message_uuid` from
//! `session.rs` during the Goal 221 module refactor.

use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

// Crate-internal so the test suite can construct custom sentinel files
// for stale-lock recovery tests.
pub(crate) const SESSION_LOCK_FILE: &str = ".lock";

// ---------------------------------------------------------------------------
// Session lock (Goal 151)
// ---------------------------------------------------------------------------

/// Error type carried inside [`std::io::Error::other`] when
/// [`SessionLock::acquire`] refuses because another live process
/// holds the lock.
#[derive(Debug, Clone)]
pub struct SessionLockBusy {
    pub pid: u32,
    pub hostname: String,
    pub started_at_unix: u64,
    pub session_dir: PathBuf,
}

impl std::fmt::Display for SessionLockBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session at {} is being written by another process \
             (pid {}, host {}, started {}). \
             If you believe this is stale, remove {}/{} and retry.",
            self.session_dir.display(),
            self.pid,
            self.hostname,
            self.started_at_unix,
            self.session_dir.display(),
            SESSION_LOCK_FILE,
        )
    }
}

impl std::error::Error for SessionLockBusy {}

/// Parsed contents of a `.lock` sentinel file.
///
/// `pub(crate)` so the test suite can construct custom sentinels for
/// stale-lock recovery / cross-host abort tests.
pub(crate) struct SentinelInfo {
    pub(crate) pid: u32,
    pub(crate) hostname: String,
    pub(crate) started_at_unix: u64,
}

impl SentinelInfo {
    /// Build a sentinel describing this process. Newlines and
    /// carriage returns in the hostname are stripped so they can't
    /// break the line-delimited format.
    fn for_self() -> Self {
        Self {
            pid: std::process::id(),
            hostname: current_hostname(),
            started_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let pid: u32 = lines.next()?.trim().parse().ok()?;
        let hostname = lines.next()?.trim().to_string();
        let started_at_unix: u64 = lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
        Some(Self {
            pid,
            hostname,
            started_at_unix,
        })
    }

    pub(crate) fn serialise(&self) -> String {
        format!(
            "{}\n{}\n{}\n",
            self.pid, self.hostname, self.started_at_unix
        )
    }
}

/// Return our hostname as a single-line string with newlines stripped
/// (so it can't break the `\n`-delimited sentinel format).
///
/// Tries `$HOSTNAME` / `$COMPUTERNAME` first, then falls back to
/// invoking `hostname(1)`. Cheap (~1ms) and only called at lock
/// acquire/release time.
pub(crate) fn current_hostname() -> String {
    let raw = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
        });
    raw.replace(['\n', '\r'], "_").trim().to_string()
}

/// Check whether `pid` is alive on the local host using `kill(2)`
/// with signal 0 (the "exists?" probe).
///
/// Implementation: spawns `/bin/kill -0 <pid>`. This avoids needing
/// a `libc` direct dependency and is fast enough at lock-acquire
/// time. On non-Unix platforms, conservatively assumes alive — we
/// would rather refuse a resume than corrupt a session. Power
/// users on those platforms can remove `.lock` manually.
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // On Linux, /proc/<pid> exists iff the process is alive.
        // This avoids the ambiguity of kill(1) exit codes for out-of-range PIDs.
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        // tasklist /FI "PID eq <pid>" /NH /FO CSV prints a CSV row like
        //   "name.exe","<pid>","..."
        // when the process exists.  When no process matches it prints
        // "INFO: No tasks are running …".  For out-of-range or otherwise
        // unrecognised PIDs (e.g. u32::MAX) the output may be empty or
        // contain only an error line — not the "INFO: No tasks" sentinel.
        // Use a positive check (look for the PID as a quoted CSV field)
        // instead of the fragile negative check so that all non-matching
        // output is treated as dead.  If `tasklist` is unavailable fall
        // back to the conservative "assume alive" policy.
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains(&format!("\"{pid}\""))
            }
            Err(_) => true,
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = pid;
        true
    }
}

/// RAII guard preventing concurrent writes to the same JSONL session.
///
/// Owns the sentinel file `<session_dir>/.lock` for the lifetime of
/// the guard. On `Drop`, the sentinel is best-effort removed.
///
/// Stale-lock recovery: if the sentinel exists, its hostname matches
/// ours, and the recorded pid is **not** alive, `acquire` overwrites
/// it (logging a warning to stderr). If the hostname differs,
/// `acquire` refuses regardless of pid liveness — pid namespaces
/// across hosts aren't comparable.
///
/// **Why not `flock(2)`**: see g151 design — the sentinel approach
/// gives a better error message ("pid 12345 on host X is still
/// running") and avoids NFS / iCloud / Dropbox flakiness.
#[derive(Debug)]
pub struct SessionLock {
    lock_path: PathBuf,
}

impl SessionLock {
    /// Acquire the lock for `session_dir`. Returns
    /// [`std::io::Error`] wrapping a [`SessionLockBusy`] when
    /// another live process holds it (or when an unrecoverable
    /// cross-host lock is detected).
    pub fn acquire(session_dir: &Path) -> std::io::Result<Self> {
        use std::fs::OpenOptions;
        use std::io::Write;

        let lock_path = session_dir.join(SESSION_LOCK_FILE);

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let info = SentinelInfo::for_self();
        let sentinel = info.serialise();

        // Attempt an atomic exclusive create: succeeds only if the file does
        // not exist, eliminating the TOCTOU window between `is_file()` and
        // `write()` that allowed two concurrent processes to both believe they
        // held the lock.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                f.write_all(sentinel.as_bytes())?;
                return Ok(Self { lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Fall through: inspect the existing sentinel.
            }
            Err(e) => return Err(e),
        }

        // The file exists. Read it and decide whether to steal (stale) or
        // refuse (live owner on same or different host).
        match std::fs::read_to_string(&lock_path) {
            Ok(text) => {
                if let Some(existing) = SentinelInfo::parse(&text) {
                    let our_host = current_hostname();
                    if existing.hostname != our_host || is_pid_alive(existing.pid) {
                        return Err(std::io::Error::other(SessionLockBusy {
                            pid: existing.pid,
                            hostname: existing.hostname,
                            started_at_unix: existing.started_at_unix,
                            session_dir: session_dir.to_path_buf(),
                        }));
                    }
                    // Stale: pid is dead on our host. Recover by overwriting.
                    eprintln!(
                        "warning: recovered stale session lock at {} \
                         (pid {} not running)",
                        lock_path.display(),
                        existing.pid,
                    );
                }
                // Parse failed — corrupt sentinel. Treat as recoverable.
            }
            Err(_) => {
                // Read failed — treat as recoverable (e.g. race-removed).
            }
        }

        // Overwrite the stale/corrupt sentinel.  Another concurrent process
        // may have beaten us to the steal; that is an accepted edge case for
        // the stale-recovery path (both processes were racing on a dead pid).
        std::fs::write(&lock_path, &sentinel)?;
        Ok(Self { lock_path })
    }

    /// Path to the sentinel file. Mostly useful for tests / error
    /// messages.
    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

// ---------------------------------------------------------------------------
// UUID chain recovery
// ---------------------------------------------------------------------------

/// Read the UUID of the last message-type (TranscriptEntry) line in a JSONL
/// file. Skips compact_boundary system entries. Returns `None` if the file
/// is empty, unreadable, or all entries lack a UUID (pre-g155 files).
pub(crate) fn read_last_message_uuid(jsonl_path: &Path) -> Option<String> {
    let file = std::fs::File::open(jsonl_path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut last = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<super::serialize::TranscriptEntry>(&line) {
            if !entry.uuid.is_empty() {
                last = Some(entry.uuid);
            }
        }
    }
    last
}

// ---------------------------------------------------------------------------
// Transcript truncation
// ---------------------------------------------------------------------------

/// Truncate `transcript.jsonl` (and the session's `.meta.json`
/// `message_count`) so that only the messages from turns
/// `0..cutoff_turn` survive.
///
/// "Turn N" is defined as the N-th non-system, non-tool user message
/// in the transcript (0-indexed). The system prompt (if any) and any
/// seed messages preceding the first user turn are always preserved.
///
/// Used by `recursive sessions rewind --to-turn N` to keep transcript
/// state in sync with the workspace state restored from a checkpoint.
pub fn truncate_transcript_to_turn(
    session_dir: &Path,
    cutoff_turn: usize,
) -> std::io::Result<TruncateStats> {
    let jsonl_path = session_dir.join("transcript.jsonl");
    if !jsonl_path.exists() {
        return Ok(TruncateStats {
            kept: 0,
            dropped: 0,
        });
    }

    // Stream-read so we don't load the whole transcript into memory.
    let file = std::fs::File::open(&jsonl_path)?;
    let reader = std::io::BufReader::new(file);

    let tmp_path = jsonl_path.with_extension("jsonl.rewind-tmp");
    let tmp = std::fs::File::create(&tmp_path)?;
    let mut writer = BufWriter::new(tmp);

    let mut user_seen = 0usize;
    let mut kept = 0u64;
    let mut dropped = 0u64;
    let mut stop = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if stop {
            dropped += 1;
            continue;
        }

        // Peek role without full deserialisation.
        let role = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("role").and_then(|r| r.as_str()).map(str::to_string));

        let is_turn_boundary = matches!(role.as_deref(), Some("user"));
        if is_turn_boundary {
            if user_seen >= cutoff_turn {
                // This user message starts the turn we're rewinding;
                // drop it and everything after.
                stop = true;
                dropped += 1;
                continue;
            }
            user_seen += 1;
        }

        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        kept += 1;
    }
    writer.flush()?;
    drop(writer);

    std::fs::rename(&tmp_path, &jsonl_path)?;

    // Update .meta.json message_count if present.
    let meta_path = session_dir.join(".meta.json");
    if meta_path.exists() {
        if let Ok(bytes) = std::fs::read(&meta_path) {
            if let Ok(mut meta) = serde_json::from_slice::<super::SessionMeta>(&bytes) {
                meta.message_count = kept;
                meta.updated_at = super::chrono_lite_now();
                if let Ok(json) = serde_json::to_string_pretty(&meta) {
                    let _ = crate::atomic::atomic_write(&meta_path, json.as_bytes());
                }
            }
        }
    }

    Ok(TruncateStats { kept, dropped })
}

/// Stats returned by [`truncate_transcript_to_turn`].
#[derive(Debug, Clone, Copy)]
pub struct TruncateStats {
    pub kept: u64,
    pub dropped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use crate::session::{SessionReader, SessionStatus, SessionWriter};

    // -- SessionLock tests --------------------------------------------------

    #[test]
    fn lock_dead_pid_recovered() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp.path().join("session-B");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Forge a stale lock with a pid that's almost certainly
        // dead. We use u32::MAX which is well past any valid pid
        // on Linux/macOS (PID_MAX_LIMIT is 2^22 by default).
        let stale = SentinelInfo {
            pid: u32::MAX,
            hostname: current_hostname(),
            started_at_unix: 0,
        };
        std::fs::write(session_dir.join(SESSION_LOCK_FILE), stale.serialise()).unwrap();

        // Recovery should succeed and overwrite the sentinel.
        let lock = SessionLock::acquire(&session_dir).unwrap();
        let raw = std::fs::read_to_string(lock.path()).unwrap();
        let parsed = SentinelInfo::parse(&raw).unwrap();
        assert_eq!(parsed.pid, std::process::id());
    }

    #[test]
    fn lock_cross_host_aborts() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp.path().join("session-C");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Forge a lock from a different host. Even though the pid
        // is dead, cross-host pid checks aren't safe → refuse.
        let cross = SentinelInfo {
            pid: u32::MAX,
            hostname: "definitely-not-our-host-123".to_string(),
            started_at_unix: 0,
        };
        std::fs::write(session_dir.join(SESSION_LOCK_FILE), cross.serialise()).unwrap();

        let err = SessionLock::acquire(&session_dir).expect_err("cross-host should fail");
        assert!(
            err.to_string().contains("definitely-not-our-host-123"),
            "expected cross-host error to mention recorded host, got: {err}"
        );
    }

    #[test]
    fn lock_released_on_drop() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp.path().join("session-D");
        std::fs::create_dir_all(&session_dir).unwrap();

        let lock = SessionLock::acquire(&session_dir).unwrap();
        assert!(lock.path().exists());
        drop(lock);
        // Drop must remove the sentinel file.
        assert!(
            !session_dir.join(SESSION_LOCK_FILE).exists(),
            "sentinel must be removed on Drop"
        );

        // A fresh acquire should succeed.
        let _lock2 = SessionLock::acquire(&session_dir).unwrap();
    }

    #[test]
    fn lock_alive_pid_blocks_acquire() {
        let tmp = crate::test_util::IsolatedWorkspace::new();
        let session_dir = tmp.path().join("session-A");
        std::fs::create_dir_all(&session_dir).unwrap();

        // First acquire succeeds; lock file now holds OUR pid.
        let lock = SessionLock::acquire(&session_dir).unwrap();

        // Second acquire by the same process: pid is alive (it's
        // us!), so it must refuse.
        let err = SessionLock::acquire(&session_dir).expect_err("second acquire should fail");
        // Match the inner SessionLockBusy via Display.
        assert!(
            err.to_string()
                .contains(&format!("pid {}", std::process::id())),
            "expected error to mention our pid {}, got: {}",
            std::process::id(),
            err
        );

        drop(lock);
    }

    // -- truncate_transcript_to_turn tests -----------------------------------

    #[test]
    fn truncate_transcript_to_turn_drops_at_user_boundary() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        // Sequence: system, user(turn 0), assistant, user(turn 1),
        // assistant, user(turn 2), assistant.
        w.append(&Message::system("sys".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u0".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a0".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u1".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a1".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u2".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a2".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();

        let session_dir = w.session_dir().to_path_buf();

        // Rewind to turn 1 → keep system + u0 + a0; drop u1 onwards.
        let stats = truncate_transcript_to_turn(&session_dir, 1).unwrap();
        assert_eq!(stats.kept, 3);
        assert_eq!(stats.dropped, 4);

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[1].role, "user");
        assert_eq!(entries[1].content, "u0");
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(entries[2].content, "a0");

        // Meta should reflect the new count.
        let meta = SessionReader::load_meta(&session_dir).unwrap();
        assert_eq!(meta.message_count, 3);
    }

    #[test]
    fn truncate_transcript_to_zero_drops_all_turns_keeps_system() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        let mut w = SessionWriter::create(dir.path(), "g", "m", "p").unwrap();
        w.append(&Message::system("sys".to_string()), None, None)
            .unwrap();
        w.append(&Message::user("u0".to_string()), None, None)
            .unwrap();
        w.append(&Message::assistant("a0".to_string()), None, None)
            .unwrap();
        w.finish(SessionStatus::Completed).unwrap();
        let session_dir = w.session_dir().to_path_buf();

        let stats = truncate_transcript_to_turn(&session_dir, 0).unwrap();
        assert_eq!(stats.kept, 1, "system message should remain");
        assert_eq!(stats.dropped, 2);

        let entries = SessionReader::load_transcript(&session_dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "system");
    }

    #[test]
    fn truncate_transcript_missing_file_is_noop() {
        let dir = crate::test_util::IsolatedWorkspace::new();
        // No session created → no transcript.jsonl. Should not panic.
        let stats = truncate_transcript_to_turn(dir.path(), 5).unwrap();
        assert_eq!(stats.kept, 0);
        assert_eq!(stats.dropped, 0);
    }
}

//! Per-session sentinel lock (Goal 151).
//!
//! Pulled out of `session.rs` so the file there focuses on
//! [`crate::session::SessionWriter`] / `SessionReader` semantics. The
//! implementation is unchanged — see the blame on `session.rs` for
//! historical context. All names re-exported from `crate::session` so
//! external paths like `recursive::session::SessionLock` keep working.

use std::path::{Path, PathBuf};

// Crate-internal so the test suite under `src/session.rs` can construct
// custom sentinel files for stale-lock recovery tests.
pub(crate) const SESSION_LOCK_FILE: &str = ".lock";

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
/// `pub(crate)` so the test suite in `src/session.rs` can construct custom
/// sentinels for stale-lock recovery / cross-host abort tests.
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
        let lock_path = session_dir.join(SESSION_LOCK_FILE);

        if lock_path.is_file() {
            match std::fs::read_to_string(&lock_path) {
                Ok(text) => {
                    if let Some(info) = SentinelInfo::parse(&text) {
                        let our_host = current_hostname();
                        if info.hostname != our_host {
                            return Err(std::io::Error::other(SessionLockBusy {
                                pid: info.pid,
                                hostname: info.hostname,
                                started_at_unix: info.started_at_unix,
                                session_dir: session_dir.to_path_buf(),
                            }));
                        }
                        if is_pid_alive(info.pid) {
                            return Err(std::io::Error::other(SessionLockBusy {
                                pid: info.pid,
                                hostname: info.hostname,
                                started_at_unix: info.started_at_unix,
                                session_dir: session_dir.to_path_buf(),
                            }));
                        }
                        // Stale: pid dead on our host. Recover.
                        eprintln!(
                            "warning: recovered stale session lock at {} \
                             (pid {} not running)",
                            lock_path.display(),
                            info.pid,
                        );
                    }
                    // Parse failed — corrupt sentinel. Treat as
                    // recoverable (overwrite) rather than abort.
                }
                Err(_) => {
                    // Read failed — treat as recoverable.
                }
            }
        }

        // (Re)write sentinel with our info.
        let info = SentinelInfo::for_self();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&lock_path, info.serialise())?;

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

#[cfg(test)]
mod tests {
    //! Tests that exercise the sentinel internals live here. The
    //! "live PID blocks acquire" case stays in `session.rs` because it
    //! only needs the public API.

    use super::*;

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
}

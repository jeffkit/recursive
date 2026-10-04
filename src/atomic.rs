//! Atomic file write helper — single source of truth for
//! "write-then-rename" persistence across the recursive codebase.
//!
//! POSIX guarantees `rename(2)` is atomic. We additionally call
//! `f.sync_all()` and (best-effort) `dir.sync_all()` so the new
//! file's data and the directory entry are durable on power loss.
//! A reader that observes `path` therefore sees either the old
//! content or the full new content — never a half-written file.
//!
//! Use this for every write to durable state: session meta, cost
//! json, transcripts, checkpoint restores, memory blobs, etc.
//!
//! The temp file lives next to the target so the rename stays on
//! the same filesystem. Its name is `.tmp-{name}-{pid}-{seq}` so
//! existing log analysis and clean-up tools can recognise it after
//! a crash, and so concurrent calls in the same process to the
//! same target don't collide on the temp name (g267).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically write `bytes` to `path` via a sibling temp file + rename.
///
/// The temp file is placed in the same directory as `path` so the
/// rename stays on the same filesystem (required for atomicity on
/// most OSes). If `path` has no parent (e.g. a bare filename), the
/// temp file is created in the current working directory.
///
/// Errors from the data `sync_all()` propagate. The directory
/// `sync_all()` is best-effort — Windows doesn't support it the
/// same way, and we don't want to fail the write just because the
/// dir sync failed.
/// Monotonic counter that makes the temp-name unique across calls
/// in the same process (PID is constant within a process; without
/// this, two threads writing the same target collide on the temp
/// path and the second `rename` fails with NotFound). g267.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_impl(path, bytes, None)
}

/// Like [`atomic_write`], but create the file with `mode` (a POSIX mode,
/// e.g. `0o600`) instead of the process default.
///
/// The mode is applied at creation *and* re-applied to the temp file before
/// the rename — a post-rename `chmod` leaves the content readable under the
/// process umask for the window in between, and permanently if the process
/// dies inside it. `O_CREAT`'s mode is masked by the umask, so the explicit
/// `set_permissions` is what pins the exact mode; creating with the mode is
/// what keeps the file private while it is still empty.
///
/// The mode is ignored on non-Unix platforms, where the process ACL model does
/// not expose a POSIX mode.
pub fn atomic_write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    atomic_write_impl(path, bytes, Some(mode))
}

fn atomic_write_impl(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    // The POSIX mode only exists on Unix; elsewhere the parameter is inert.
    #[cfg(not(unix))]
    let _ = mode;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".tmp-{}-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        seq,
    ));

    // Write to temp, fsync the data, then rename.
    {
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // fsync the parent dir so the rename is durable. Best-effort:
    // Windows may not support this, and a tmp file outside a
    // directory we can open (e.g. a chrooted test) is still safe
    // — the next `rename` will just not be dir-fsynced.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Async [`atomic_write_with_mode`] for async call sites.
///
/// Uses `tokio::task::spawn_blocking` under the hood so the blocking
/// fs work does not stall the async runtime. Takes `&Path` (and
/// clones the path internally) so callers can keep using the
/// original path in error messages after the move. The bytes are
/// moved into the blocking task, so callers can pass an owned
/// `Vec<u8>` from async contexts without copying.
pub async fn atomic_write_async_with_mode(
    path: &Path,
    bytes: Vec<u8>,
    mode: u32,
) -> io::Result<()> {
    let owned_path = path.to_path_buf();
    tokio::task::spawn_blocking(move || atomic_write_with_mode(&owned_path, &bytes, mode))
        .await
        .map_err(|e| io::Error::other(format!("atomic_write_async join: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_atomic_write_creates_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        atomic_write(&p, b"hello, world").unwrap();
        let contents = std::fs::read(&p).unwrap();
        assert_eq!(contents, b"hello, world");
    }

    #[test]
    fn test_atomic_write_overwrites() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        atomic_write(&p, b"v1").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"v1");
        atomic_write(&p, b"v2 with more bytes").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"v2 with more bytes");
    }

    #[test]
    fn test_atomic_write_cleans_tmp_on_success() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        atomic_write(&p, b"content").unwrap();
        // After a successful atomic_write, no `.tmp-*` file may remain
        // in the parent directory. This is the load-bearing property:
        // a stray tmp would be a sign the rename never ran.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let s = name.to_str().unwrap();
            assert!(!s.starts_with(".tmp-"), "found leftover temp file: {s}");
        }
    }

    #[test]
    fn test_atomic_write_ignores_stale_tmp_orphan() {
        // A crash *between* `write_all` and `rename` leaves a `.tmp-*`
        // orphan on disk that matches the TEMP_SEQ naming pattern
        // (`.tmp-{name}-{pid}-{seq}`). The data-integrity contract
        // (module doc): a reader observing `path` sees either the old
        // content or the full new content — never a half-written file —
        // and a previous crash's leftover must NOT corrupt the next write.
        //
        // PINNED ACTUAL BEHAVIOUR: `atomic_write` never scans the parent
        // directory for pre-existing temp files; it only writes its own
        // fresh `.tmp-{name}-{pid}-{seq}` and renames it. So the stale
        // orphan is IGNORED (still present, content untouched), not
        // cleaned up. This test asserts the no-corruption guarantee; it
        // does NOT promise orphan reaping (that would be a separate
        // `cleanup_tmp_orphans` helper goal).
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("target.txt");
        atomic_write(&p, b"v1").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"v1");

        // Simulate a crashed writer's leftover: a temp file matching the
        // `.tmp-{name}-{pid}-{seq}` pattern but from a DIFFERENT pid/seq
        // than the live process would generate, so it can't collide with
        // the temp name the next `atomic_write` picks.
        let stale = dir.path().join(format!(
            ".tmp-target.txt-{}-{}",
            std::process::id() + 1, // pid of the crashed writer ≠ this process
            999_999,                // seq far beyond TEMP_SEQ's small counter
        ));
        std::fs::write(&stale, b"half-written garbage from a crashed process").unwrap();

        // The next write must succeed and leave a well-formed target.
        atomic_write(&p, b"v2").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"v2", "target must not be torn");

        // The stale orphan is harmless: ignored, not renamed over the
        // target, and its content untouched.
        assert!(
            stale.exists(),
            "stale orphan is ignored (not reaped) by atomic_write"
        );
        assert_eq!(
            std::fs::read(&stale).unwrap(),
            b"half-written garbage from a crashed process",
            "stale orphan content must be untouched"
        );
    }

    #[test]
    fn test_atomic_write_empty_bytes() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("empty.txt");
        atomic_write(&p, b"").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
    }

    #[test]
    fn test_atomic_write_nested_path() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a").join("b").join("c.txt");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        atomic_write(&p, b"nested").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"nested");
    }

    #[test]
    fn test_atomic_write_concurrent_writes_serialise_via_pid() {
        // Two writes to the *same* target should produce the same temp
        // name (PID is stable for the test). The second write truncates
        // the temp, then both rename — the final state is the bytes
        // from whichever rename ran last. We don't assert order
        // (that's racy) but we assert the result is one of the two
        // valid payloads, not a torn write.
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("race.txt");
        let dir_path = dir.path().to_path_buf();
        let h1 = std::thread::spawn(move || {
            atomic_write(&dir_path.join("race.txt"), b"AAAA").unwrap();
        });
        let h2 = std::thread::spawn(move || {
            atomic_write(&p, b"BBBB").unwrap();
        });
        h1.join().unwrap();
        h2.join().unwrap();
        let contents = std::fs::read(dir.path().join("race.txt")).unwrap();
        assert!(
            contents == b"AAAA" || contents == b"BBBB",
            "torn write: {:?}",
            contents
        );
    }

    #[tokio::test]
    async fn test_atomic_write_async_roundtrip() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("async.txt");
        atomic_write_async_with_mode(&p, b"async bytes".to_vec(), 0o600)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"async bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "async write must apply the mode too");
        }
    }

    /// Issue #102 review: the mode must be on the file when it appears at
    /// `path` — a post-rename chmod leaves a window (and a crash leaves the
    /// plaintext world-readable for good).
    #[cfg(unix)]
    #[test]
    fn test_atomic_write_with_mode_is_private_from_the_rename_on() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("secret.jsonl");
        atomic_write_with_mode(&p, b"secret", 0o600).unwrap();

        assert_eq!(std::fs::read(&p).unwrap(), b"secret");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the renamed file must be owner-only, got {:o}",
            mode & 0o777
        );
        // No temp file may linger with the content either.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let s = entry.unwrap().file_name();
            assert!(
                !s.to_string_lossy().starts_with(".tmp-"),
                "found leftover temp file: {s:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_atomic_write_async_concurrent() {
        // Async wrapper under multiple tasks should still produce
        // well-formed output for each task (they don't share a path,
        // so no contention on the temp name).
        let dir = TempDir::new().unwrap();
        let mut handles = Vec::new();
        for i in 0..4u8 {
            let d = dir.path().to_path_buf();
            handles.push(tokio::task::spawn_blocking(move || {
                let p = d.join(format!("file_{i}.txt"));
                atomic_write(&p, format!("payload-{i}").as_bytes()).unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        for i in 0..4u8 {
            let p = dir.path().join(format!("file_{i}.txt"));
            assert_eq!(
                std::fs::read(&p).unwrap(),
                format!("payload-{i}").as_bytes()
            );
        }
    }
}

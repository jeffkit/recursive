//! Cross-module test helpers.
//!
//! This module is compiled in two situations:
//! 1. `cfg(test)` — for unit tests inside `src/**`.
//! 2. `feature = "test-utils"` — for integration tests under `tests/**`
//!    and for external consumers (examples, downstream crates).
//!
//! Both compilations share the **same statics**, which is the entire
//! point: process-global state (env vars, current dir, signal handlers)
//! that the tests mutate must serialise across every binary linked
//! against this crate. A `Mutex` defined per-module would not — each
//! `cargo test` worker still shares one process for unit + integration
//! tests, so a single `lib.rs`-level static is the only place where a
//! lock can sit and actually be one lock.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Process-global lock for tests that mutate or read environment
/// variables that affect path resolution (`HOME`, `RECURSIVE_HOME`,
/// `XDG_*`, etc.).
///
/// All such tests **must** acquire this guard for the entire span of
/// their env-mutation + assertions. Tests that only *read* env-derived
/// state (e.g. anything that calls `crate::paths::user_data_dir`) also
/// need to hold it to avoid observing a torn-down `tempdir` that some
/// other test had pointed `RECURSIVE_HOME` at.
///
/// The lock is poison-tolerant — a test panicking while holding it
/// must not poison the lock for unrelated test runs.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// RAII guard that pins `RECURSIVE_HOME` to a given path for the
/// guard's lifetime, while holding the global env lock. Restores the
/// previous value (or removes it) on drop.
///
/// Use in tests that need an isolated per-user data root:
///
/// ```ignore
/// let tmp = tempfile::tempdir().unwrap();
/// let _g = recursive::test_util::PinnedRecursiveHome::new(tmp.path());
/// // ... code that reads RECURSIVE_HOME ...
/// ```
pub struct PinnedRecursiveHome {
    _guard: MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
}

impl PinnedRecursiveHome {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        let guard = env_lock();
        let prev = std::env::var_os("RECURSIVE_HOME");
        // SAFETY: `set_var` is process-global; we hold the env lock so
        // no other test mutates env concurrently.
        unsafe {
            std::env::set_var("RECURSIVE_HOME", path.as_ref().as_os_str());
        }
        Self {
            _guard: guard,
            prev,
        }
    }
}

impl Drop for PinnedRecursiveHome {
    fn drop(&mut self) {
        // SAFETY: still hold the lock until `_guard` drops after this.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("RECURSIVE_HOME", v),
                None => std::env::remove_var("RECURSIVE_HOME"),
            }
        }
    }
}

/// Same as [`PinnedRecursiveHome`] but for `HOME`.
pub struct PinnedHome {
    _guard: MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
}

impl PinnedHome {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        let guard = env_lock();
        let prev = std::env::var_os("HOME");
        // SAFETY: see `PinnedRecursiveHome::new`.
        unsafe {
            std::env::set_var("HOME", path.as_ref().as_os_str());
        }
        Self {
            _guard: guard,
            prev,
        }
    }
}

impl Drop for PinnedHome {
    fn drop(&mut self) {
        // SAFETY: still hold the lock until `_guard` drops after this.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

/// One-stop helper: a workspace tempdir paired with a `RECURSIVE_HOME`
/// pinned at a sibling tempdir, both alive for the bundle's lifetime.
///
/// Use this in any test that calls into code which resolves paths via
/// `crate::paths::user_*` (e.g. `ShadowRepo::open`, `SessionWriter`,
/// scratchpad). Without it, parallel tests that briefly redirect
/// `RECURSIVE_HOME` or `HOME` to *their* tempdirs (and then drop them)
/// can corrupt path resolution mid-test.
///
/// `path()` returns the workspace dir — the part the test usually
/// wants. Drop order: workspace tempdir → home tempdir → env unpin
/// (releases the global env lock last).
pub struct IsolatedWorkspace {
    workspace: tempfile::TempDir,
    _home: tempfile::TempDir,
    _pin: PinnedRecursiveHome,
}

impl IsolatedWorkspace {
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let pin = PinnedRecursiveHome::new(home.path());
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        Self {
            workspace,
            _home: home,
            _pin: pin,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        self.workspace.path()
    }
}

impl Default for IsolatedWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

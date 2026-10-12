//! Crate-wide process-environment guard for `recursive-cli`'s unit tests.
//!
//! The `--bin recursive` unit tests all run in **one process**, so a test that
//! mutates a process-global env var is visible to every sibling test running in
//! parallel. Before issue #185 the only env guard lived `#[cfg(test)]`-private
//! inside [`crate::cli::builder`], while `main.rs` and `cli/resume.rs` tests
//! mutated the same variables with raw `std::env::set_var` — so
//! `unknown_preset_env_fails_the_cli_build` (`RECURSIVE_AGENT_PRESET`) raced
//! `build_runtime_*` readers and turned the gate red at random.
//!
//! Every test that mutates **or** depends on an env var routes through
//! [`EnvGuard`]: one crate-wide lock serialises the mutation *and* the
//! env-sensitive reads, and the previous value is restored on drop.
//!
//! The lock is a `std::sync::Mutex` (not a tokio one) so a plain `#[test]` and
//! a `#[tokio::test]` serialise against each other. Holding it across an
//! `.await` is deliberate: the guard only ever contends with sibling tests of
//! this same binary, so blocking one test thread cannot stall another test's
//! runtime.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

/// Process-global lock shared by every env-touching test in this binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII env guard: holds [`ENV_LOCK`] for its whole lifetime and restores
/// every variable it touched on drop.
pub(crate) struct EnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Take the crate-wide env lock **without** mutating anything.
    ///
    /// Use it in a test that only *reads* env-derived state (e.g.
    /// `build_runtime` resolving `RECURSIVE_AGENT_PRESET`): without the lock
    /// the read can land inside another test's transient set/restore window.
    pub(crate) fn lock() -> Self {
        EnvGuard {
            saved: Vec::new(),
            _lock: lock_env(),
        }
    }

    /// Take the lock and apply `vars` (`None` removes the variable), recording
    /// each variable's pre-test value for restoration on drop.
    pub(crate) fn set(vars: &[(&str, Option<&str>)]) -> Self {
        let mut guard = Self::lock();
        for (k, v) in vars {
            match v {
                Some(val) => guard.set_var(k, val),
                None => guard.remove_var(k),
            }
        }
        guard
    }

    /// Set one variable while holding the lock. The first mutation of a
    /// variable remembers its pre-test value; later mutations of the same
    /// variable must not overwrite that snapshot.
    pub(crate) fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, key: K, value: V) {
        self.remember(key.as_ref());
        // SAFETY: `set_var` mutates process-global state; `ENV_LOCK` is held
        // for this guard's whole lifetime, so no sibling test is mid-read, and
        // `Drop` restores the prior value.
        unsafe {
            std::env::set_var(key, value);
        }
    }

    /// Remove one variable while holding the lock (see [`Self::set_var`]).
    pub(crate) fn remove_var<K: AsRef<OsStr>>(&mut self, key: K) {
        self.remember(key.as_ref());
        // SAFETY: see `set_var`.
        unsafe {
            std::env::remove_var(key);
        }
    }

    fn remember(&mut self, key: &OsStr) {
        if self.saved.iter().any(|(k, _)| k.as_os_str() == key) {
            return;
        }
        self.saved.push((key.to_os_string(), std::env::var_os(key)));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            // SAFETY: `ENV_LOCK` is still held (the `_lock` field drops after
            // this body), so the restore cannot race another test.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn lock_env() -> MutexGuard<'static, ()> {
    // Poison-tolerant: a test panicking while holding the guard must not
    // poison the lock for the rest of the binary.
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Guard self-tests ─────────────────────────────────────────────────────

/// The guard is load-bearing: while one is alive the crate-wide lock must be
/// held, and dropping it must release the lock. If `EnvGuard` ever stops
/// locking (the #185 negative control), this test fails immediately instead of
/// leaving the flake to manifest as a random red run.
#[test]
fn env_guard_holds_the_crate_wide_lock() {
    let guard = EnvGuard::set(&[("RECURSIVE_TEST_ENV_GUARD_PROBE", Some("1"))]);
    assert_eq!(
        std::env::var("RECURSIVE_TEST_ENV_GUARD_PROBE").as_deref(),
        Ok("1")
    );
    assert!(
        ENV_LOCK.try_lock().is_err(),
        "a live EnvGuard must hold the crate-wide ENV_LOCK"
    );
    drop(guard);
}

/// The guard restores the pre-test value (including "was unset").
#[test]
fn env_guard_restores_the_previous_value_on_drop() {
    const KEY: &str = "RECURSIVE_TEST_ENV_GUARD_RESTORE";
    let before = std::env::var_os(KEY);
    {
        let mut guard = EnvGuard::lock();
        guard.set_var(KEY, "transient");
        assert_eq!(std::env::var(KEY).as_deref(), Ok("transient"));
        guard.remove_var(KEY);
        assert!(std::env::var_os(KEY).is_none());
    }
    assert_eq!(
        std::env::var_os(KEY),
        before,
        "drop must restore the prior value"
    );
}

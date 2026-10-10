//! Process-level hardening (issue #150).
//!
//! Issue #89 scrubbed credential-shaped variables out of *child* command
//! environments, but the service process kept its own execve-time
//! environment intact. On Linux that block is mirrored at
//! `/proc/<pid>/environ`, so a prompt-injected child — the default tier
//! spawns `/bin/sh` as a direct child, making the service process its
//! `$PPID` — could still walk off with every credential:
//!
//! ```sh
//! tr '\0' '\n' < /proc/$PPID/environ
//! ```
//!
//! `std::env::remove_var` cannot close this: glibc's `unsetenv` only edits
//! the C `environ` array, never the kernel's original environment block.
//! The fix is to clear the process's *dumpable* attribute at startup:
//! `prctl(PR_SET_DUMPABLE, 0)` makes the kernel expose
//! `/proc/<pid>/environ` (and `mem`/`maps`) as root-owned, so a same-uid
//! child's `open()` fails with `EACCES` and only `CAP_SYS_PTRACE` holders
//! can still read it.
//!
//! Call [`harden_process`] once, as early as possible — before any
//! execution surface (tool transports, `run_background`, the HTTP server)
//! can spawn a child. The CLI does this at the top of its `main()`;
//! library embedders must call it themselves after process start.
//!
//! Trade-off: `PR_SET_DUMPABLE=0` also disables core dumps for the process.
//! That is intentional — a core dump would contain the same secrets — and
//! is not a regression.

/// Make this process's environment (and memory) unreadable to same-uid
/// children on Linux.
///
/// No-op on non-Linux targets: macOS has no `/proc/<pid>/environ` channel
/// (reading another task's environment there requires the
/// `task_for_pid` entitlement), so this issue does not apply.
///
/// Idempotent: calling it more than once is harmless.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl` is a plain syscall wrapper. `PR_SET_DUMPABLE`
        // takes one integer argument; passing zero is the documented
        // "not dumpable" setting. The return value is deliberately
        // ignored: there is no meaningful recovery, and an unhardened
        // process is still fully functional (issue #89's env scrub
        // remains in place as the second layer).
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn harden_process_clears_dumpable_and_is_idempotent() {
        super::harden_process();

        // SAFETY: `PR_GET_DUMPABLE` takes no argument and only reads the
        // per-process dumpable flag.
        let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        assert_eq!(dumpable, 0, "PR_SET_DUMPABLE did not take effect");

        // A second call must not panic and must leave the flag set.
        super::harden_process();
        // SAFETY: as above.
        let again = unsafe { libc::prctl(libc::PR_GET_DUMPABLE) };
        assert_eq!(
            again, 0,
            "second harden_process() changed the dumpable flag"
        );
    }

    /// Non-Linux targets keep a compiling, callable no-op (macOS has no
    /// `/proc`). Exercises the function so the cross-platform arm is
    /// covered too.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn harden_process_is_a_noop_off_linux() {
        super::harden_process();
        super::harden_process();
    }
}

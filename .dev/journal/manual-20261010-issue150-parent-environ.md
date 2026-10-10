# Issue #150 — hide the service process's `/proc/<pid>/environ` from children

**Date**: 2026-10-10
**Goal**: #150 (security/sandbox) — #89's credential scrub is bypassable: a
same-uid child reads the service process's execve-time environment out of
`/proc/$PPID/environ`, exfiltrating every inbound credential / upstream LLM
key into the transcript.

## Root cause

`scrub_child_env` (`src/tools/transport_layer/transport.rs`) only rewrites
the **child's** env block. `LocalTransport::exec_shell` spawns `/bin/sh`
directly from the service process, so `$PPID` is the service process itself
and `tr '\0' '\n' < /proc/$PPID/environ` dumps its environment. `remove_var`
cannot close this: glibc's `unsetenv` edits the C `environ` array, never the
kernel environment block established at `execve`.

## Files touched

- `src/hardening.rs` (new) — `pub fn harden_process()`; on Linux calls
  `prctl(PR_SET_DUMPABLE, 0)` so the kernel exposes `/proc/<pid>/environ`
  as root-owned and a same-uid child's `open()` fails with `EACCES`. No-op
  off Linux (macOS has no `/proc`; reading another task's env needs the
  `task_for_pid` entitlement). Unit tests: dumpable flag is cleared,
  idempotent; plus a no-op test for non-Linux.
- `src/lib.rs` — `pub mod hardening;`.
- `Cargo.toml` — `[target.'cfg(target_os = "linux")'.dependencies] libc = "0.2"`.
- `crates/recursive-cli/src/main.rs` — `recursive::hardening::harden_process()`
  as the first statement of `main()`, before `Cli::parse()` /
  `Config::from_env()` and before any execution surface can spawn a child.
  Also adds `mod tests::harden_process_is_idempotent` (the CLI `src/` change
  needs a test-bearing diff for the `cli-presence` gate; the harness is
  unchanged otherwise).
- `tests/issue89_parent_environ.rs` (new, `#![cfg(target_os = "linux")]`).
- `docs/architecture/execution-environments.md`, `.dev/AGENTS.md` (invariant
  #3 extension, issue #150).

## Tests added

- `tests/issue89_parent_environ.rs::hardened_process_hides_its_environ_from_same_uid_children`
  — **positive control** first: an unhardened same-uid grandchild reads its
  parent's `/proc/$PPID/environ` and finds `RECURSIVE_API_KEY=issue150-leak-me`
  (proves the channel is real / the assertion is not vacuous). Then
  `harden_process()` and run the issue's exact command through the real
  `Bash` tool (`RunShell`): the observation must not contain
  `RECURSIVE_API_KEY`/`RECURSIVE_HTTP_AUTH_*` and must not show `exit: 0`
  (the open was refused). Skips when `geteuid() == 0` — `PR_SET_DUMPABLE` is
  ignored for a caller with `CAP_SYS_PTRACE`, so root is documented out of
  scope (see the issue's "陷阱").
- `src/hardening.rs` unit tests (invariant #4).
- `crates/recursive-cli/src/main.rs::tests::harden_process_is_idempotent`.

## Notes

- **The issue's sketch said `prctl(22, 0, …)` / "PR_SET_DUMPABLE = 22"; that
  literal is wrong.** The kernel value is `PR_SET_DUMPABLE = 4`
  (`PR_GET_DUMPABLE = 3`; 22 is `PR_SET_SECCOMP`). The implementation calls
  `libc::PR_SET_DUMPABLE` / `libc::PR_GET_DUMPABLE` from the `libc` crate
  (verified against libc 0.2.186 source), so it cannot drift from the kernel
  ABI. Passing the literal from the sketch would have set SECCOMP, not the
  dumpable bit.
- **Invariant #6 (new dependency):** `libc` was not a direct dependency, but
  it is already in `Cargo.lock` (0.2.186, transitive via tokio/reqwest), and
  the dependency is scoped to the `target.'cfg(target_os = "linux")'` table.
  It adds no new crate to the graph; the only lockfile change is an edge on
  the `recursive-agent` package. `libc` is the minimal way to call
  `prctl(PR_SET_DUMPABLE)` without hand-rolling a syscall.
- **Why not `remove_var` (the issue's suggestion #1):** explicitly documented
  as not the fix — it does not rewrite `/proc/<pid>/environ`. It is also
  unsafe here: `RECURSIVE_HTTP_AUTH_KEYS` / `RECURSIVE_HTTP_AUTH_JWT_SECRET`
  are read lazily during HTTP startup (`auth_config_from_env`), so removing
  them after `Config::from_env` would silently break HTTP auth. The
  dumpable bit is the complete fix; #89's child-env scrub stays as the
  second layer (not touched, per the issue's "不做").
- **Trade-off:** `PR_SET_DUMPABLE=0` disables core dumps for the process.
  Documented in `src/hardening.rs` and the architecture doc so it is not
  later mistaken for a regression.
- **Embedders:** the CLI hardens itself; library embedders must call
  `recursive::hardening::harden_process()` after process start (documented).

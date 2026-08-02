# Manual edit: goal-360 — Kill child + drain reader tasks on SSH/shell timeout

**Date**: 2026-08-02
**Goal**: Close the resource leak in the timeout `Err(_)` arms of the
`run_shell`-style paths. Both `ssh_exec`/`exec_shell` (SSH transport) and the
`Bash` tool (shell.rs) spawn a child + two `tokio::spawn` reader tasks, then
`tokio::time::timeout(child.wait())`. On timeout they returned `Err`
immediately — SSH never killed the child (remote process kept running until the
connection dropped) and both paths detached the reader JoinHandles (tasks parked
on `read_capped()` until the pipe closes).

## Files touched

- `src/tools/transport.rs` — TWO SSH timeout arms fixed (the goal's snippet
  shows `ssh_exec` at ~:231, but `SshTransport::exec_shell` at ~:406 has the
  identical "SSH command timed out" leak, so both were fixed):
  - `ssh_exec` (`self.command_timeout` arm): replaced the bare `return Err(...)`
    with
    `let _ = child.start_kill(); let _ = child.wait().await; let _ =
    stdout_task.await; let _ = stderr_task.await;` then the same `Err(TimedOut)`.
  - `SshTransport::exec_shell` (`timeout` param arm): same kill→wait→drain.
  - Test module: added `// TODO(goal-360)` comment explaining why the SSH
    timeout arm can't be e2e-tested (no SSH server in test env; `ssh
    user@host` fails fast with connection refused, never reaching the timeout)
    and that the contract is pinned by the shell.rs test + grep.
- `src/tools/shell.rs` — timeout arm already called `child.start_kill()` (good);
  added `let _ = child.wait().await;` (reap) and the two reader awaits before
  `return Err(...)`, plus a comment explaining kill-before-drain ordering.

`LocalTransport::exec_shell` (~transport.rs:505) has the same leak class but was
left untouched — the goal scopes strictly to the SSH transport + shell.rs
("do exactly this, no more"). Flagging for a follow-up goal: its timeout arm
also returns without killing the child or draining the readers, and it's the
DEFAULT transport (registry.rs). Also note: that arm's existing test
`local_transport_exec_shell_timeout` passes only because it never inspects the
orphaned child.

## Tests added

- `src/tools/shell.rs` — `shell_timeout_drains_reader_tasks`: runs
  `exec sleep 30` with a 200ms timeout, asserts `execute()` returns
  `Error::Tool` containing "timed out" within 5s. Uses `exec` (established idiom
  from `timeout_kills_child_process`) so the shell PID *becomes* the sleeper and
  the kill closes the pipes → drain completes immediately. A plain `sleep 30`
  would leave an orphaned descendant holding the pipes and the drain would block
  for the full 30s (see Notes). If kill were dropped or the drain ordered before
  the kill, this test hangs → fails.
- transport.rs: no new runnable SSH test (infeasible without a server); the
  TODO comment documents this. Existing 36 transport tests still pass.

## Notes / traps

- **Order matters**: readers block on the pipes until the child dies, so
  kill (+wait) MUST precede the reader awaits. Awaiting readers first deadlocks.
- **`exec sleep` trap**: `/bin/sh -c "sleep 30"` forks sleep, so killing sh
  orphans sleep which still holds the stdout/stderr write ends → the drain would
  block until sleep exits. This is a KNOWN limitation of the best-effort cleanup
  on shell.rs: if a timed-out command leaves descendants holding the tool's
  pipes, `execute()` will not return until those pipes close. The goal's
  rationale ("the kill eventually closes the pipes so the reader tasks do exit")
  holds for the SSH path (killing the ssh client drops the connection and closes
  its local pipes) and for `exec`-style shell commands, but not for forked
  descendants. A future hardening could use `setsid`/process groups to kill the
  whole tree. Documented in the test comment; accepted per goal scope.
- **Both SSH arms fixed, not one**: the goal's "Why" cites `transport.rs:228-236`
  (`ssh_exec`) but the identical leak exists in `SshTransport::exec_shell`
  (~:398). The acceptance criterion ("the Err(_) arm ... in transport.rs
  contains start_kill and stdout_task.await") and the goal title ("SSH/shell
  timeout") cover both; leaving one would be a half-fix.
- **`let _ =` on the drain is intentional** (error path, best-effort, don't
  surface a secondary cleanup error).
- **Happy paths untouched** — both files still await the reader tasks the same
  way on the `Ok` arm.
- `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and
  `cargo test --workspace` all clean.

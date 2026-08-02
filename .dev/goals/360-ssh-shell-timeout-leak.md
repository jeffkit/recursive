# Goal 360 — Kill child + drain reader tasks on SSH/shell timeout (resource leak)

**Roadmap**: Tools / resource correctness — timeout paths orphan processes and detach tasks

**Design principle check**:
- Implemented as: adding kill + join to two timeout `Err(_)` arms (one in `transport.rs`,
  one in `shell.rs`). No new capability; closes a resource-handling gap.
- ❌ Does NOT touch the agent kernel, run loop, or invariants. Pure tool-internal cleanup.
- No new deps.

## Why (the leak, with evidence)

Both `run_shell`-style tools spawn a child + two reader tasks (`tokio::spawn` to read
stdout/stderr into buffers), then `tokio::time::timeout` the `child.wait()`. On timeout
they return `Err` immediately — **without killing the child or awaiting the reader tasks**:

**SSH transport** — `src/tools/transport.rs:228-236`:
```rust
let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, max).await });
let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, max).await });
let wait = child.wait();
let status = match tokio::time::timeout(self.command_timeout, wait).await {
    Ok(s) => s?,
    Err(_) => {
        return Err(std::io::Error::new(           // ← returns immediately
            std::io::ErrorKind::TimedOut,
            format!("SSH command timed out after {:?}", self.command_timeout),
        ));
    }
};
// (the happy path DOES await stdout_task/stderr_task below)
```
On timeout: (1) the remote SSH child is **never killed** (`start_kill` not called) — it
keeps running on the remote host until the connection drops or it finishes; (2)
`stdout_task`/`stderr_task` are **detached** (their JoinHandles drop) — they park on
`read_capped().await` until the SSH pipe closes (which it won't, because the child is
alive), leaking the task slots for the process lifetime.

**shell.rs** — `src/tools/shell.rs:169-188`: the timeout arm DOES call
`child.start_kill()` (good) but then `return`s without awaiting `stdout_task`/`stderr_task`.
The kill eventually closes the pipes so the reader tasks do exit, but any truncated output
is silently lost and the task slots transiently leak under load. Less severe than SSH but
the same class.

## Scope (do exactly this, no more)

### 1. Fix SSH transport timeout arm (`src/tools/transport.rs:231-235`)

Replace the bare `return Err(...)` with kill-then-drain:

```rust
Err(_) => {
    // Kill the child so the SSH connection drops and reader tasks unblock;
    // then drain them so their JoinHandles don't detach with buffered output.
    let _ = child.start_kill();
    let _ = child.wait().await;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    return Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("SSH command timed out after {:?}", self.command_timeout),
    }));
}
```
Mirror the kill→wait→drain ordering of the happy path below. The `_ = ` discards are
intentional: we're on the error path and want best-effort cleanup, not to surface a
secondary error from the drain.

### 2. Fix shell.rs timeout arm (`src/tools/shell.rs` ~:182-188)

Same pattern — after `child.start_kill()`, await the two reader tasks before returning:
```rust
child.start_kill();
let _ = child.wait().await;   // if applicable to shell.rs's child type
let _ = stdout_task.await;
let _ = stderr_task.await;
return Err(...);
```
Read the actual shell.rs code at the timeout arm first — confirm the child type and
whether `wait()` is available (it's likely `tokio::process::Child`). Match whatever the
happy path uses.

### 3. Tests

- **`ssh_transport_timeout_kills_child_and_drains_readers`** (or extend an existing
  transport test): this is hard to test end-to-end without a real SSH server. A pragmatic
  unit test: construct the transport with a very short `command_timeout` against a
  long-running command (e.g. `sleep 30` via a local mock, or a stub if real SSH isn't
  available in tests), trigger the timeout, and assert the function returns `Err(TimedOut)`
  promptly (within a few seconds, NOT 30s). If real-SSH tests aren't feasible in CI, at
  minimum add a test that verifies the timeout arm's *behaviour contract* via a
  shorter-lived local process child using the same code path. Read existing transport tests
  for the established pattern.
- **`shell_timeout_drains_reader_tasks`**: in `src/tools/shell.rs`'s test module, add a
  test that runs `sleep 30` (or `sleep 2`) with a 200ms timeout and asserts: (a) returns
  `Err` with TimedOut kind promptly, (b) does not hang. The kill+drain correctness is
  implied by "returns promptly without leaking". Check existing shell tests for the
  timeout-test idiom.

If end-to-end testing of the SSH path is genuinely infeasible (no SSH server in test env),
focus the test on shell.rs (which CAN be tested locally) and add a `// TODO` + journal note
for SSH. The fix is still correct-by-construction (mirrors the happy-path ordering); the
test on shell.rs pins the pattern.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- The happy-path (Ok arm) of either file — it already awaits correctly; don't refactor it.
- Other tools (`web_fetch`, `docker_sandbox`, etc.) — the `tokio::spawn` in `Drop` issue
  (docker/e2b) is a SEPARATE concern; do NOT conflate it into this goal.
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green, including any new timeout tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Manual reasoning: the SSH timeout arm now calls `start_kill` + awaits both reader tasks
  before returning (grep-verifiable: the `Err(_)` arm of the `tokio::time::timeout` match
  in transport.rs contains `start_kill` and `stdout_task.await`).

## Notes for the agent (traps)

- **Order matters: kill BEFORE drain.** The reader tasks are blocked on `read_capped()`
  reading from the child's stdout/stderr pipes. They will not unblock until the pipe closes,
  which happens when the child dies (and the OS closes its fds). So you MUST `start_kill`
  (+ `wait`) first, THEN `await` the reader tasks. Awaiting readers before kill deadlocks.
- **`start_kill` vs `kill`.** `tokio::process::Child::start_kill` is non-blocking (sends
  SIGTERM, returns immediately); `kill` is async. Use `start_kill` then `wait().await` to
  reap. Do NOT use `.kill().await` blindly — read the actual child type.
- **`let _ = ` on the drain is correct.** On the error path we don't care if a reader task
  already panicked or the pipe is broken; we just want to not detach it. Don't `?` the
  drain.
- **Best-effort, not transactional.** If `start_kill` itself fails (process already gone),
  swallow it and still attempt the drain. The goal is "no orphaned child + no detached
  task", not "perfect cleanup guarantees".
- **Don't change the happy path.** The `Ok(s) => s?` arm flows into the existing
  `stdout_task.await` / `stderr_task.await` below; that's correct. Only the timeout arm is
  broken.

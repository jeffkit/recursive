# Goal 379 — Fix background-task lifecycle leaks: task_stop abort skips deregister; multi-job completion loses wakeup

**Roadmap**: Kernel / task lifecycle — two resource-leak/lost-wakeup bugs in the
background-task machinery (Goal-370 leak class, abort path; and run_event_loop dropping
completions when ≥2 jobs finish at once)

**Design principle check**:
- Implemented as: a deregister-on-abort fix in the task registry / task_stop path + a
  drain-loop fix in the runtime's completion handling. No new deps. No public API
  changes.
- ❌ Does NOT touch the agent kernel invariants or the run loop's structure — the
  completion handling change is inside `run_event_loop`'s existing "background job
  completed" arm.

## Why (both verified 2026-08-03 by reading the code)

**1. `task_stop` abort skips deregistration — WorkerTable leak (Goal-370 class).**
`src/tools/task_stop.rs` (`execute`) → `task.stop().await` → `src/tasks.rs:183`
`handle.abort()` aborts the worker's JoinHandle. The worker's own deregistration lives
inside the spawned task at `src/tools/agent.rs:658` `reg.deregister(&worker_id).await`
(after `run_worker` completes; the comment at `agent.rs:510` says "Clone the table + key
so the task can deregister itself on EVERY [exit path]" — but **abort unwinds the task**,
skipping that line). Result: after `task_stop`, the `WorkerHandle` (mpsc sender) stays in
the process-wide `WorkerTable` forever; a later `send_message` to that id "succeeds"
while silently buffering into a dead unbounded channel. Goal 370 fixed the normal-exit
leak; the abort path re-leaks.

**2. `notify_one` + single `take_completed()` loses wakeups when ≥2 jobs finish.**
`src/tools/run_background.rs:156` notifies completion with `notify_one()` (comment at
`:119-120` explains it stores a single permit). `src/runtime.rs:1362` handles a wake with
`if let Some((id, output)) = mgr.take_completed()` — one job per wake. When ≥2 background
jobs complete while the arbiter is mid-turn, both `notify_one()` calls coalesce into one
permit; the arbiter wakes, drains ONE job, consumes the permit — the second job sits in
the completed queue with no further wake scheduled → the loop idles forever even though a
background job is finished (agent waits on a wake that never comes).

## Scope (do exactly this, no more)

### 1. Deregister on abort

Make the abort path remove the worker from the registry, idempotently and
leak-free. Preferred approach: wrap the worker body (in `src/tools/agent.rs`, around
`:658`) in a scope-exit guard so deregistration runs on BOTH normal exit and abort
(e.g. a small local `struct DeregGuard(&Registry, id); impl Drop { async-free dereg }` —
if the dereg is async, use a `JoinSet`/`scopeguard`-style sync defer that spawns the
removal, or restructure so the registry removal is synchronous/`blocking_send`).
Alternatively/additionally: in `src/tools/task_stop.rs` after a successful `stop()`,
explicitly `registry.deregister(&id)` (must be safe when the worker already
deregistered — check the registry's `deregister` is idempotent, and make it so if not).

Requirements:
- After `task_stop` on a running task, `WorkerTable` no longer returns the worker;
  `send_message` to that id must produce a clear "no such worker" error instead of
  buffering silently.
- Normal-exit deregistration behavior (Goal 370) must be unchanged and still tested.
- `task_stop` on an already-finished/terminal task stays a no-op.

### 2. Drain all completed jobs per wake

In `src/runtime.rs:1362` (the `run_event_loop` background-completion arm): replace
`if let Some(...) = mgr.take_completed()` with a `while let Some(...)` drain that handles
every currently-completed job in one wake (queue a turn/notify for each, or fold them
into one turn if that matches the loop's semantics — read the surrounding code and keep
the per-job turn behavior identical to today's single-job path). Also audit
`src/tools/run_background.rs:119-156`: if the drain loop makes `notify_one()`'s permit
semantics safe, keep it; otherwise switch to `notify_waiters()` (only one arbiter waits)
or re-notify when the queue is non-empty after draining. The invariant to establish:
**no completed job is ever left in the queue without a scheduled wake**.

### 3. Tests

- `task_stop` on a running task → registry no longer contains the worker id; a
  subsequent `send_message` to it fails with a worker-not-found error (extend the
  existing `stop_running_task` test in `src/tools/task_stop.rs` and/or the
  `background_worker_deregisters_from_worker_table_on_exit` test in `agent.rs:1478`).
- Background-job manager: enqueue TWO jobs, complete both, run the arbiter's completion
  handling → both are observed (no lost wakeup). If `run_event_loop` itself is hard to
  drive in this goal, test at the manager+notify level: complete 2 jobs, assert
  `take_completed()` drains both and the notify permits/queue end empty — but prefer a
  `run_event_loop`-level test if the existing harness allows it (see goal 381 which adds
  run_event_loop driver tests; don't block on it).

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs` — kernel invariants.
- `src/tools/a2a.rs`, `src/tools/web_fetch.rs`, `src/tools/url_guard.rs` — unrelated.
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Headline test by name: `cargo test --manifest-path Cargo.toml task_stop` and
  `cargo test --manifest-path Cargo.toml take_completed` — new tests green.
- Grep: `rg "deregister" src/tools/task_stop.rs src/tools/agent.rs src/tasks.rs` — the
  abort path now reaches a deregister (guard or explicit call).
- Grep: `rg "take_completed" src/runtime.rs` — the arm drains in a loop (≥2 hits:
  `while let` + fn def).

## Notes for the agent (traps)

- **Abort is asynchronous**: `JoinHandle::abort()` signals cancellation; the task unwinds
  on its next await point. A Drop guard on the worker's stack runs during unwind — that
  is why a scope-exit guard (not a trailing call) is the robust fix. If deregistration is
  async, do the registry removal synchronously (the table is likely a `Mutex<HashMap>` —
  lock + remove is fine) rather than awaiting in Drop.
- **Idempotence**: `deregister` may be called twice (guard on abort + normal path) — make
  it a no-op for missing keys and keep `stop_running_task`-style tests green.
- **Do not change the notify mechanism's timing semantics** for the single-job case —
  existing tests (`take_completed_returns_finished_job` at `run_background.rs:764`,
  `:797`) must stay green unchanged.
- **Read the whole `run_event_loop` completion arm** before editing — understand how a
  completed job becomes a new turn (goal injection) so the drain loop preserves
  per-job turn behavior.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-<date>-goal379-task-lifecycle-leaks.md`.

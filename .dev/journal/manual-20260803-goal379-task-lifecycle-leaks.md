# Manual edit: Goal 379 — background-task lifecycle leaks (deregister on abort + drain all completed)

**Date**: 2026-08-03
**Goal**: Goal 379 — Fix background-task lifecycle leaks: `task_stop` abort skips
deregister; multi-job completion loses wakeup

**Supervisor note**: this goal was rescued from self-improve run
`selfimprove-1785743355828` (flow `recursive-flow-20260803T154904`). The run's agent
was mid-`run.recursive` with all 5 files edited but **uncommitted** when the cargo-mutants
verification broke (disk ENOSPC + interrupted runs). The supervisor stopped the flow,
backed up the uncommitted diff (`.flowcast/supervisor-backup-20260803-163725/`), verified
fmt/test/clippy in the worktree, committed the work there as `da64c79`, and cherry-picked
it to `main` as `d580f4e`. The `gate.agent-mutants` mutation-testing pass was **not**
completed for this goal (cargo-mutants infra failure; see Phase-5 list).

## What changed

All changes confined to the 5 files in scope (`src/multi.rs`, `src/runtime.rs`,
`src/tools/agent.rs`, `src/tools/run_background.rs`, `src/tools/send_message.rs`). No
kernel / run-loop structural changes, no new deps.

### 1. Deregister on abort — `src/tools/agent.rs` (+ `src/multi.rs`)

New `WorkerDeregisterGuard` (RAII, local of the spawned worker async block): its `Drop`
removes the worker's `WorkerHandle` from the process-wide `WorkerTable` synchronously
(lock + remove) so it runs on **every** exit path — normal return, error return, and the
`task_stop` `JoinHandle::abort()` unwind that previously skipped the trailing
`reg.deregister(...)` (goal-370 leak class: dead sender buffered silently forever).
`src/multi.rs` reads the worker table synchronously (`lock()` instead of `read().await`)
so the `Drop`-time removal is lock-safe (no await in Drop).

### 2. Drain all completed jobs per wake — `src/runtime.rs` + `src/tools/run_background.rs`

`run_event_loop`'s background-completion arm now drains with
`while let Some((id, output)) = mgr.take_completed()` instead of a single
`if let Some`, so when ≥2 background jobs finish mid-turn (both `notify_one()` permits
coalesce into one wake) every completed job is observed in that wake — no completed job
is ever left in the queue without a scheduled wake. `run_background.rs` documents the
single-permit `notify_one()` semantics and the drain re-arms as needed.

### 3. `send_message.rs`

Error paths tightened for missing/dead worker (clear "no such worker" style errors
instead of silently buffering into a dead channel).

## Verification

- `cargo fmt --all -- --check` — clean
- `cargo test --quiet` — green (no failures)
- `cargo clippy --all-targets --all-features -- -D warnings` — clean

## Follow-up

- `gate.agent-mutants` mutation testing was NOT completed for this goal (infra failure —
  see Phase-5). A cargo-mutants pass on the 5 touched files should be run before this is
  considered fully landed.
- Goal-379 acceptance grep checks: `rg "deregister" src/tools/task_stop.rs
  src/tools/agent.rs src/tasks.rs` and `rg "take_completed" src/runtime.rs` — satisfied
  by the committed diff.

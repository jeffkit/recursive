# Manual edit: goal-370 — Deregister background `WorkerHandle` from `WorkerTable` on exit

**Date**: 2026-08-02
**Goal**: Fix the resource leak where a spawned background worker's `WorkerHandle`
(holding an `mpsc::UnboundedSender` + `Arc<...>`) lingers in the process-wide
`WorkerTable` for the lifetime of the process after the worker task exits.

## Files touched

- `src/tools/agent.rs`
  - `spawn_background_worker` (spawn block, ~line 507-547): before the
    `tokio::spawn(async move { ... })`, capture `let workers = self.workers.clone();`
    and `let worker_key = worker_id.to_string();` (same key as the insert at
    `self.workers.write().await.insert(worker_id.to_string(), ...)`). Inside the task
    body, add `let _ = workers.write().await.remove(&worker_key);` on **both** normal
    exit paths:
    1. `Err(e)` turn-failure branch — after `state_for_task.mark_failed(...).await;`
       and before `return;` (line ~536).
    2. End-of-loop channel-close branch — after `state_for_task
       .mark_completed("worker finished".to_string()).await;` (line ~545).
  - Panic path intentionally NOT covered (panic unwinds past the removal lines;
    documented in a code comment and the test comment, per goal's "minimum viable"
    instruction — don't over-engineer panic recovery).
  - `tokio::sync::RwLock` — `.await` on `write()` required, no std lock introduced.

## Tests added

- `src/tools/agent.rs` `#[cfg(test)] mod tests`:
  - `background_worker_deregisters_from_worker_table_on_exit` — behavioral test:
    builds an `AgentTool` with a `MockProvider` whose error queue returns an
    `Error::Llm` on the first `complete()` call, spawns a background worker
    (`mode: single`, `background: true`), asserts the entry `w1` is present while
    alive, then polls (bounded, 5s) until the entry disappears. Also asserts the
    task reached `TaskStatus::Failed` (confirming the `mark_failed` exit path was
    exercised). **Verified fails-on-old-code**: temporarily removed the two
    cleanup lines and the test panicked with
    `worker table entry for 'w1' was not removed after the worker exited` after the
    5s timeout; restored the fix and it passes in 0.02s.

## Verification

- `cargo fmt --all` — clean.
- `cargo test -p recursive-agent --lib tools::agent` — 28/28 pass (incl. the new test).
- `cargo test --workspace` — green (see run).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean (see run).
- `rg "workers\.write\(\)\.await\.remove" src/tools/agent.rs` → 2 hits (both exit paths).

## Notes / judgment calls

- **Option A chosen** (goal's preferred pattern): the task body has exactly two
  normal exit points, so a one-liner at each reads cleaner than an RAII drop guard
  (which can't be async anyway) or a restructure into an inner `async` block.
- **Channel-close path is hard to trigger in a behavioural test**: the only
  `UnboundedSender` for a background worker lives inside the `WorkerHandle` in the
  table, so the channel can only close if the entry is removed first — on old code
  that path was effectively unreachable in practice (the mark_failed path is the
  real-world leak). The regression test therefore drives the **mark_failed** path,
  which is genuinely reachable and fails on old code; the channel-close path shares
  the same removal line and is covered by the code change itself.
- **`task_stop` note**: `task_stop` aborts the JoinHandle (`TaskState::stop` →
  `handle.abort()`); abort is a panic-style unwind inside the spawned task, so the
  removal lines are skipped and a stopped worker can still leave a stale entry.
  Goal explicitly scoped this out ("note it in the journal but don't expand scope").
  If desired later, `TaskState::stop` could be extended to also remove the
  WorkerTable entry — but that touches `src/tasks.rs` / the `task_stop` tool, out of
  scope here.
- The legacy `WorkerRegistry` / mailbox `deregister` (`:647`, `send_message.rs:98`)
  is a different structure and was left untouched, per goal scope.

# Goal 370 — Deregister background `WorkerHandle` from `WorkerTable` on exit (resource leak)

**Roadmap**: Tools / multi-agent — `WorkerTable` grows unbounded for process lifetime

**Design principle check**:
- Implemented as: a `workers.write().await.remove(&id)` in every exit path of the spawned
  background worker task in `src/tools/agent.rs`, plus a regression test.
- ❌ Does NOT touch the agent kernel, `run_core.rs`, or any invariant. The worker lifecycle
  behaviour is unchanged for live workers; only *dead* workers stop lingering in the table.
- No new deps.

## Why (the leak, with evidence)

`src/tools/agent.rs:191`:
```rust
pub type WorkerTable = Arc<RwLock<HashMap<String, Arc<WorkerHandle>>>>;
```

The spawned background worker task (`src/tools/agent.rs:510` region, the
`tokio::spawn(async move { ... })` for `background: true` agents) inserts into this table
at **`src/tools/agent.rs:506`**:
```rust
workers.write().await.insert(worker_id.to_string(), handle.clone());
```

… but on **no exit path does it remove the entry.** The worker task can end three ways:
1. `mark_failed(...)` → early `return` (e.g. at `:527`),
2. the inbound `rx` channel closes → `mark_completed(...)` (e.g. at `:533`),
3. panic.

In all three, the `WorkerHandle` (which holds an `mpsc::UnboundedSender` clone + an
`Arc<...>`) stays in `self.workers` for the rest of the process. **Verified**: `grep -n
'workers\..*remove\|\.remove(' src/tools/agent.rs` finds the table is only ever inserted
into; the `deregister` call at `:647` (and `send_message.rs:98`) targets the *legacy*
`WorkerRegistry` mailbox — a different structure, not `WorkerTable`.

**Impact.** A long-running coordinator / HTTP / TUI process that spawns many background
agents (the intended use of coordinator mode) accumulates dead `WorkerHandle`s and their
`UnboundedSender`s indefinitely — a slow memory + channel-buffer leak. Secondary smell:
`send_message(task_id=...)` iterating the stale table may match a `WorkerHandle` whose
task is already dead (it handles the `tx.send` error, but still — stale data).

## Scope (do exactly this, no more)

### 1. Remove the entry on every worker exit path (`src/tools/agent.rs`)

In the spawned worker task (the `tokio::spawn(async move { ... })` block starting near
`:510`), ensure the `WorkerTable` entry for `worker_id` is removed when the task ends —
on **all** of: `mark_failed` early-return, channel-close `mark_completed`, and panic.

Cleanest pattern: capture a clone of the table + id before the spawn, and remove in a
`finally`-style position. Two implementation options — pick whichever reads cleanest given
the existing code shape (READ the spawn block first):

- **Option A (struct after the spawn body):** since the task body has multiple exit points
  (`return` after `mark_failed`, end-of-loop after `mark_completed`), wrap the whole thing
  so every path hits the removal. The simplest is to compute the removal as the **last**
  statement before each `return`/end-of-block. If there are 2-3 exit points, just add the
  one-liner at each.

- **Option B (drop guard):** introduce a tiny RAII guard whose `Drop` does
  `workers.write().await.remove(...)` — but `Drop` can't be async, so this only works if
  you can use a `std::sync::RwLock` write or a `try_lock`. **Prefer Option A** unless the
  task body is so branchy that A is ugly.

Use:
```rust
// Capture before the task body's terminal moves, on EVERY exit path:
let _ = workers.write().await.remove(worker_id);
```
(`worker_id` is the `String`/`&str` key used at insert `:506` — match it exactly. `let _ =`
because `remove` returns the old value we don't need.)

**Panic path:** a `tokio::spawn`'d task that panics aborts only that task (not the
process, in default tokio). To clean up on panic too, wrap the task body in
`AssertUnwindSafe(...).catch_unwind()` OR — simpler and the idiomatic tokio way — accept
that a panicked worker leaves a stale entry and add a `workers.write().await.remove` in a
small `tokio::spawn` cleanup if practical. **Minimum viable:** cover the two normal exit
paths (mark_failed return, channel-close mark_completed); note the panic case in the test
comment. Don't over-engineer panic recovery.

### 2. Regression test (`src/tools/agent.rs` `#[cfg(test)]` module)

Add a test that:
- Constructs an `AgentPool` (or whatever the existing tests use to set up a
  `WorkerTable` — see the test helpers at `:1351` / `:1408` which build
  `WorkerTable = Arc::new(RwLock::new(HashMap::new()))`).
- Triggers a background worker to start AND end (channel close is the easiest end:
  drop the sender side, let the worker see the channel close → `mark_completed`).
- Awaits the worker task's completion.
- Asserts: `workers.read().await.contains_key(&worker_id)` is **false** (entry removed).

The test must FAIL on the old code (entry still present after worker exits) and PASS on
the new code. If driving a real background worker end-to-end in a test is heavy, the
minimum viable version: extract the cleanup logic so it's directly callable, and assert
calling it removes the entry. But prefer the behavioural test.

Read the existing tests at `:1351` and `:1408` first to mirror their setup harness.

## Files NOT to touch

- `src/run_core.rs`, `src/kernel.rs`, `src/runtime.rs` — unrelated.
- The legacy `WorkerRegistry` / mailbox (`:647`, `src/tools/send_message.rs:98`) — that's a
  *different* structure; its deregister is intentional and fine. Don't unify the two.
- The insert at `:506` — insert must stay; only add the symmetric remove.
- `src/tools/send_message.rs` — out of scope (it's the consumer; the fix is at the producer).
- `.dev/flows/`.

## Acceptance

- `cargo test --workspace` green.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- The new regression test passes and fails-on-old-code (verify by reasoning — the test
  asserts the entry is GONE after the worker exits, which is only true post-fix).
- Grep: `rg "workers\.write\(\)\.await\.remove" src/tools/agent.rs` returns **≥1 hit** in
  the worker task body (the new removal call).

## Notes for the agent (traps)

- **Match the key exactly.** The insert at `:506` uses `worker_id.to_string()` as the key.
  The remove must use the same key (same string value). If `worker_id` is an `&str` in
  scope, `.to_string()` again, or capture the owned `String` clone used for insert.
- **`RwLock` is tokio's, not std's.** `workers.write().await` — the `.await` is required
  (it's `tokio::sync::RwLock`, given the `Arc<RwLock<...>>` + async context). Don't reach
  for `std::sync::RwLock` or you'll deadlock.
- **Don't remove on `task_stop`.** The `task_stop` tool already aborts the JoinHandle; it's
  a *user-initiated* stop. The fix here is for *natural* task exit (failure/completion).
  If `task_stop` also leaves a stale entry, that's a separate concern — note it in the
  journal but don't expand scope. (Though if your removal lives in a place both paths hit,
  fine.)
- **The `deregister` at `:647` is NOT this table.** It's the legacy `WorkerRegistry`
  (mailbox). Confirmed via grep — don't conflate the two.
- **Read the spawn block first.** The exact line numbers (`:506`, `:510`, `:527`, `:533`)
  are from a 2026-08-02 read; verify against current code before editing. The structure
  (insert, then spawn task with mark_failed-return and mark_completed end) is what matters.

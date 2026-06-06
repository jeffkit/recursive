# Goal 244 — drain_queue() should not swallow per-message errors

**Roadmap**: Arch-review bugfixes (high severity)

**Design principle check**:
- Implemented as: change return type / stop-on-first-error in `drain_queue`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`AgentRuntime::drain_queue()` processes queued messages but silently logs
and discards errors from individual `execute_kernel_turn()` calls. Callers
cannot distinguish which messages completed vs failed. In multi-agent
scenarios a failed message may leave stale state that corrupts subsequent
turns.

## Scope (do exactly this, no more)

### 1. `src/runtime.rs` — stop on first error in `drain_queue`

Read the current `drain_queue` implementation. It likely loops over queued
messages and calls `execute_kernel_turn` for each, logging errors and
continuing. Change it to stop on the first error and return that error to
the caller:

```rust
// Before (roughly):
pub async fn drain_queue(&mut self) -> usize {
    let mut processed = 0;
    while let Some(msg) = self.queue.pop_front() {
        match self.execute_kernel_turn(...).await {
            Ok(_) => processed += 1,
            Err(e) => { tracing::error!(...); } // silently continues
        }
    }
    processed
}

// After:
pub async fn drain_queue(&mut self) -> Result<usize> {
    let mut processed = 0;
    while let Some(msg) = self.queue.pop_front() {
        self.execute_kernel_turn(...).await?;
        processed += 1;
    }
    Ok(processed)
}
```

If `drain_queue` already returns `Result`, just ensure it propagates errors
instead of logging-and-continuing. Read the actual implementation before
editing.

### 2. Update all callers of `drain_queue`

Search for all call sites of `drain_queue` in `src/` and update them to
handle the `Result`. Typically:
- If called in a background task, log the error and break.
- If called in a request handler, propagate the error.

Use `grep -rn "drain_queue" src/` to find all callers.

### 3. Tests

Add a test in `src/runtime.rs` `#[cfg(test)]` that enqueues two messages
where the first will succeed and the second would also succeed, verifies
`drain_queue` returns `Ok(2)`. Then separately verify that if a turn errors,
`drain_queue` returns `Err` and does not process subsequent messages.

Use the existing test helpers / mock infrastructure already present in
`src/runtime.rs` tests.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `drain_queue` returns `Result<usize>` and propagates the first error
- All existing callers compile and handle the result

## Notes for the agent

- Read `src/runtime.rs` `drain_queue` implementation before editing.
- Read all callers with `grep -rn "drain_queue" src/`.
- If the queue is a `VecDeque`, note that items not yet processed should
  remain in the queue when returning early on error (don't drain items
  that weren't processed).
- **DO NOT modify** `src/agent.rs`, `src/llm/`, `src/config.rs`, `src/http/`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** You are running
  headless; the plan gate has no reviewer. Just read and edit directly.

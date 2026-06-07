# Goal 259 — Fix `drain_queue` message loss on error (M-3)

**Roadmap**: Code quality — architecture review follow-up (P2 backlog)

**Design principle check**:
- Implemented as: small targeted bug fix in `src/runtime.rs::drain_queue`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Architecture review (`docs/review/architecture-review-2026-06-07.md`,
item M-3) identified a data-loss bug in the message-queue drain path.
At `src/runtime.rs:591-598`:

```rust
async fn drain_queue(&mut self) -> Result<Option<RuntimeOutcome>> {
    let mut last: Option<RuntimeOutcome> = None;
    while let Some(msg) = self.message_queue.pop_front() {
        let outcome = self.run(msg).await?;  // <-- pop happens BEFORE run
        last = Some(outcome);
    }
    Ok(last)
}
```

`pop_front()` returns the message and removes it from the queue
**before** `run()` is called. If `run()` returns an error (transient
LLM failure, network error, provider rate limit), the message is
permanently lost — the caller has no way to retry. The existing test
at `src/runtime.rs:2127` even codifies the bug as expected behavior
("second message was already popped before the error").

This is a P2 data-integrity issue: a transient error during turn B
silently drops message B. The correct contract is **pop only on
success** (or hold a `current_processing` slot until the call
returns).

## Scope (do exactly this, no more)

### 1. `src/runtime.rs::drain_queue` — pop only on success

Replace the current implementation with one that does not lose the
in-flight message on error. Two acceptable shapes; pick the smaller
one:

**Option A — peek then pop (preferred)**:

```rust
async fn drain_queue(&mut self) -> Result<Option<RuntimeOutcome>> {
    let mut last: Option<RuntimeOutcome> = None;
    while let Some(msg) = self.message_queue.front().cloned() {
        match self.run(msg).await {
            Ok(outcome) => {
                // pop only after success
                self.message_queue.pop_front();
                last = Some(outcome);
            }
            Err(e) => {
                // message still at front of queue; bubble up the error
                return Err(e);
            }
        }
    }
    Ok(last)
}
```

**Option B — guard pattern**:

```rust
async fn drain_queue(&mut self) -> Result<Option<RuntimeOutcome>> {
    let mut last: Option<RuntimeOutcome> = None;
    loop {
        let Some(msg) = self.message_queue.pop_front() else { break };
        match self.run(msg).await {
            Ok(outcome) => last = Some(outcome),
            Err(e) => {
                // re-prepend the in-flight message so it can be retried
                self.message_queue.push_front(/* the message we just ran */);
                return Err(e);
            }
        }
    }
    Ok(last)
}
```

Option B requires holding the message across the `run()` call (e.g.
take from queue, keep in a local, re-push on error). **Use Option A
unless Option A has a borrow-checker conflict** — Option A is simpler
and has no re-push race.

**Important**: the new `drain_queue` MUST still call `pop_front()`
exactly once per successfully-processed message (preserving the FIFO
contract and the existing test `drain_queue_returns_ok_for_all_messages`).

### 2. `src/runtime.rs` tests — invert the broken assertion

**Update `drain_queue_stops_on_first_error` (line 2110+)**:

The current test asserts `rt.queue_len() == 0` after the error. This
codifies the bug. Update it to assert the **new** correct behavior:

- The first message (which was successfully processed) IS popped.
- The second message (which caused the error) REMAINS in the queue.
- `rt.queue_len() == 1` after the error.
- The first message is gone (verify via a side-effect or by checking
  that the queue's first element is the second message).

Read the existing test setup carefully (lines 2110–2135) before
editing. The mock provider needs to return `Ok(outcome)` for the
first message and `Err(...)` for the second. If the test currently
uses a `MockProvider` that always errors, you'll need to extend it
to return success-then-error (or use two distinct messages and
inspect mock state).

**Add a new test** `drain_queue_preserves_remaining_messages_on_error`:

- Enqueue 3 messages.
- Mock returns Ok, Err, Ok.
- Assert: `queue_len() == 2` after the drain (the 2 unprocessed
  messages remain; the first one was successfully processed and
  popped).
- Assert: error is propagated.

### 3. Verify

```bash
cargo test --lib runtime::
cargo test --bin recursive
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

All must be clean. The new behavior must not regress
`drain_queue_returns_ok_for_all_messages` (FIFO + all-processed) or
`enqueue_drains_multiple_messages_in_order` (callers still see
messages processed in order).

## Acceptance

- `drain_queue` no longer loses the in-flight message on error
- The in-flight message stays at the front of the queue and can be
  retried by calling `drain_queue` again after the error is handled
- `drain_queue_stops_on_first_error` updated to assert new behavior
- New test `drain_queue_preserves_remaining_messages_on_error` added
- All other `drain_queue` callers and tests pass unchanged
- All quality gates clean

## Notes for the agent

- Read the test at `src/runtime.rs:2110-2140` in full before editing.
  The existing assertions are intricate; the change is small but
  must not break the message-A-was-processed check.
- The `MockProvider` likely has a `scripted_responses` or
  `with_response_then_error` method — read its API in the same file.
  If no such helper exists, add the smallest helper needed
  (`MockProvider::with_responses(Vec<Result<_, _>>)` or similar).
- DO NOT change the public signature of `drain_queue`. It returns
  `Result<Option<RuntimeOutcome>>` and that contract must be
  preserved.
- DO NOT change the queue type (`VecDeque<String>` or whatever it
  is) — that would balloon the diff.
- The fix is at most ~10 lines of production code. Total diff
  (test + prod) should be < 60 lines.
- If `cargo test --lib runtime::` reveals other tests that rely on
  the old "lose message on error" behavior, update them too — but
  search the entire `src/` and `tests/` trees for callers first.

## Out of scope (DO NOT do these)

- Don't add retry logic to `drain_queue` itself. Callers already
  decide when to retry. The fix is just "don't drop the message
  on error".
- Don't change `enqueue`, `run`, or the message-queue struct.
- Don't add new error variants to `src/error.rs` — reuse the
  existing `Err` propagation.
- Don't refactor `drain_queue` into a generic stream/iterator
  pattern. Keep it a `while let` loop.

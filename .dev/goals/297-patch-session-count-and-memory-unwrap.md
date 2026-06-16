# Goal 297 — Fix PATCH /sessions/:id message_count and memory.rs lock().unwrap()

**Roadmap**: Post-Phase (correctness + invariant compliance)

**Design principle check**:
- Two small fixes in separate files: one correctness bug, one invariant violation.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

### Fix 1: PATCH /sessions/:id returns message_count: 0

`src/http/handlers.rs:488` has:
```rust
message_count: 0, // omitted in patch response; caller can re-fetch if needed
```

This is incorrect. `SessionState::non_system_message_count` is an `Arc<AtomicUsize>`
that is updated in real-time. Reading it without locking the runtime is trivially
safe and gives an accurate count. Returning `0` misleads callers into thinking the
session has no messages.

### Fix 2: memory.rs `lock().unwrap()` — Invariant #5 violation

`src/tools/memory.rs:329`:
```rust
let _guard = self.lock.lock().unwrap();
```

`.unwrap()` on a `Mutex::lock()` can panic if another thread panicked while holding
the lock (mutex poisoning). Invariant #5 prohibits `.unwrap()` in production code.

## Scope (do exactly this, no more)

### Fix 1 — `src/http/handlers.rs`

In `patch_session`, change:
```rust
// Before (incorrect):
message_count: 0, // omitted in patch response; caller can re-fetch if needed

// After (correct):
message_count: session.non_system_message_count.load(std::sync::atomic::Ordering::Relaxed),
```

No other changes to `patch_session`.

### Fix 2 — `src/tools/memory.rs`

In the `execute` method of `Remember` (and any other production sites in `memory.rs`
that call `lock().unwrap()`), replace with poison recovery:
```rust
// Before:
let _guard = self.lock.lock().unwrap();

// After:
let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
```

Search `memory.rs` for all `lock().unwrap()` occurrences and apply this pattern.

### Tests

- Add (or update) a test for `PATCH /sessions/:id` that verifies the response
  `message_count` matches the actual session message count.
- The memory.rs fix has no observable behavior change, so no new test needed,
  but verify existing memory tests still pass.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `PATCH /sessions/:id` response `message_count` is the actual count (not 0)
- No `lock().unwrap()` in `src/tools/memory.rs` production code

## Notes for the agent

- Read `src/http/handlers.rs` (patch_session, SessionInfo) and
  `src/tools/memory.rs` (Remember.execute) first.
- For Fix 1: the `non_system_message_count` is already accessible on the
  session object in `patch_session` — look at how `list_sessions` reads it.
- For Fix 2: the pattern `unwrap_or_else(|p| p.into_inner())` recovers from
  mutex poisoning by returning the poisoned guard, which is safe here since
  the critical section is just a file read/write.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.

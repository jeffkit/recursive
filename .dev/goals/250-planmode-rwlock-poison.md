# Goal 250 — Fix expect() on poisoned RwLock in PlanApprovalGate / PlanModeRequestGate

**Roadmap**: Arch-review bugfixes (P0 — process crash in HTTP mode)

**Design principle check**:
- Implemented as: replace panicking `expect()` with safe lock recovery
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`src/tools/plan_mode.rs` lines 102 and 380 call `.expect("... lock poisoned")`
on `std::sync::RwLock::read()` inside async `tokio::spawn` tasks. If any
thread panics while holding the write lock, the lock becomes poisoned and the
next `read()` returns `Err`. The `expect()` then causes a secondary panic in
the background task — in HTTP server mode this kills the entire process.

This is Invariant #5 violation: `unwrap()`/`expect()` in non-test product code.

## Scope (do exactly this, no more)

### 1. `src/tools/plan_mode.rs` — lines 102 and 380

Replace both `.expect("... lock poisoned")` calls with poison-safe recovery.

In `PlanApprovalGate::wait_for_approval` (line 102):
```rust
// Replace:
let guard = self.response.read().expect("PlanApprovalGate response lock poisoned");

// With:
let guard = self.response.read().unwrap_or_else(|e| e.into_inner());
```

In `PlanModeRequestGate::wait_for_decision` (line 380):
```rust
// Replace:
let guard = self.response.read().expect("PlanModeRequestGate response lock poisoned");

// With:
let guard = self.response.read().unwrap_or_else(|e| e.into_inner());
```

The `into_inner()` on `PoisonError` recovers the guard from a poisoned lock,
allowing the caller to continue reading the (still-valid) data inside.

Also fix the `write()` calls in the same functions: they already use `if let Ok(mut w)
= self.response.write()` which silently ignores poison — that is fine (it is in
a non-critical clear step). No change needed there.

### 2. Tests

No new tests needed. The existing tests in `plan_mode.rs` (cfg(test) section)
should still pass. Confirm with `cargo test --workspace`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No `expect()` calls remain on `RwLock` outside `#[cfg(test)]` blocks in `plan_mode.rs`

## Notes for the agent

- Read `src/tools/plan_mode.rs` fully before editing.
- Only change lines 102 and 380 (the two `expect()` on `.read()`).
- Do NOT change the `if let Ok(mut w) = self.response.write()` patterns — they
  are already safe.
- Do NOT change test code (lines 540+).
- Do NOT modify any other file.
- Run `cargo test --workspace` (not just `cargo check`) before declaring done.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Running headless.

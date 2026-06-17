# Goal 315 — Fix Mutex lock().unwrap() violations in src/hooks/external.rs

## Why

Invariant #5: no `unwrap()` in non-test production code. `src/hooks/external.rs`
uses `std::sync::Mutex` for the `executed_once: Arc<Mutex<HashSet<usize>>>`
field, and calls `.lock().unwrap()` four times in production code (lines ~485,
504, 528, 550):

```rust
let guard = self.executed_once.lock().unwrap();     // line 485
self.executed_once.lock().unwrap().insert(idx);     // line 504
self.executed_once.lock().unwrap().insert(idx);     // line 528
self.executed_once.lock().unwrap().insert(idx);     // line 550
```

If the mutex is poisoned (a thread panicked while holding it), these will
panic in production — exactly what Invariant #5 prohibits.

G302 fixed the same pattern in `src/main.rs`. This goal applies the
same fix to `hooks/external.rs`.

## Scope

**File to touch**: `src/hooks/external.rs`

Replace the four `.lock().unwrap()` calls on `self.executed_once` with
`.lock().unwrap_or_else(|e| e.into_inner())` (poison recovery) or, better,
replace the `std::sync::Mutex` with a `tokio::sync::Mutex` since this
code runs inside an async context (`run_hooks` is `async`).

### Preferred approach: `unwrap_or_else(|e| e.into_inner())`

This matches the pattern used in G302 for `main.rs`:

```rust
// Before:
let guard = self.executed_once.lock().unwrap();

// After:
let guard = self.executed_once.lock().unwrap_or_else(|e| e.into_inner());
```

Apply to all four call sites.

### Alternative approach: `tokio::sync::Mutex`

If the reviewer prefers, switch `executed_once` from `std::sync::Mutex` to
`tokio::sync::Mutex`. That requires:
- Change field type declaration (line ~313)
- Change `Arc::new(Mutex::new(...))` in constructors (lines ~349, 383)
- Change all four `.lock().unwrap()` to `.lock().await`
- Update `use` imports

The `tokio::sync::Mutex` approach is cleaner for an async context but requires
`await` which may require making inner closures `async`. Use the
`unwrap_or_else` approach if `tokio::sync::Mutex` requires significant
refactoring.

## Tests

No new tests required — the existing test suite in `src/hooks/external.rs`
(lines 962+) covers hook execution. Verify that `cargo test --workspace`
passes with the change.

## Acceptance criteria

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` no diff
- Zero `.lock().unwrap()` calls on `self.executed_once` in non-test code

## Notes for the agent

- `executed_once` is a `std::sync::Mutex` (line 28: `use std::sync::{Arc, Mutex}`)
- The simplest fix is `unwrap_or_else(|e| e.into_inner())` — same as G302
- Do NOT touch any `#[cfg(test)]` code
- After patching, verify with `grep -n "executed_once" src/hooks/external.rs`
  that no `.unwrap()` remains outside test blocks

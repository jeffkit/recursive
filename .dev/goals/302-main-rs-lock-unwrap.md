# Goal 302 — Fix Mutex lock().unwrap() violations in src/main.rs

**Roadmap**: Post-Phase (Invariant #5 compliance)

**Design principle check**:
- Implemented as: replacing `lock().unwrap()` with `lock().unwrap_or_else`
  in `src/main.rs` (3 occurrences)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

**Invariant #5**: No `unwrap()`/`expect()` in non-test product code.

`src/main.rs` has three `lock().unwrap()` calls on `session_writer`'s
`std::sync::Mutex`:

```
line 1713: let session_dir = w.lock().unwrap().session_dir().to_path_buf();
line 1753: let session_id = sw.lock().unwrap().session_id().to_string();
line 1754: let session_dir = sw.lock().unwrap().session_dir().to_path_buf();
```

If a thread panics while holding the `session_writer` lock (unlikely but
possible in a multi-threaded async environment), the mutex becomes poisoned
and these `.unwrap()` calls would propagate a panic that kills the main
CLI process — exactly what Invariant #5 aims to prevent.

The fix is the same pattern used in G297 for `memory.rs`:
use `.unwrap_or_else(|e| e.into_inner())` to recover the inner value from
a poisoned mutex rather than panicking.

## Scope (do exactly this, no more)

### 1. `src/main.rs` — fix 3 occurrences

Replace each `lock().unwrap()` pattern with `lock().unwrap_or_else(|e| e.into_inner())`:

**Line ~1713:**
```rust
// Before
let session_dir = w.lock().unwrap().session_dir().to_path_buf();
// After
let session_dir = w.lock().unwrap_or_else(|e| e.into_inner()).session_dir().to_path_buf();
```

**Line ~1753:**
```rust
// Before
let session_id = sw.lock().unwrap().session_id().to_string();
// After
let session_id = sw.lock().unwrap_or_else(|e| e.into_inner()).session_id().to_string();
```

**Line ~1754:**
```rust
// Before
let session_dir = sw.lock().unwrap().session_dir().to_path_buf();
// After
let session_dir = sw.lock().unwrap_or_else(|e| e.into_inner()).session_dir().to_path_buf();
```

### 2. Scan for other `lock().unwrap()` in the same file

After fixing the 3 known occurrences, run:
```bash
grep -n "lock()\.unwrap()" src/main.rs
```
If any remain, fix them with the same pattern. If they are inside `#[cfg(test)]`
blocks, they are exempt.

### 3. Tests

No new tests needed — this is a straightforward defensive change. The
existing `cargo test` suite verifies the module still compiles and behaves
correctly.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `grep -n "lock()\.unwrap()" src/main.rs` shows zero non-test occurrences

## Notes for the agent

- Read `src/main.rs` around lines 1705–1760 for full context.
- The `session_writer` is `Arc<std::sync::Mutex<SessionWriter>>` — the
  `lock()` call returns `std::sync::LockResult<MutexGuard<SessionWriter>>`.
- `unwrap_or_else(|e| e.into_inner())` extracts the inner value from a
  `PoisonError<MutexGuard<T>>`, allowing the program to continue even if
  the mutex was poisoned.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  `src/http/`, or any other files.
- **DO NOT fix** the `ctrl_c.await.unwrap()` at line ~1522 or the
  `expect("failed to register SIGTERM handler")` at line ~1515 — signal
  handler registration failure is a process-startup fatal condition where
  panicking is intentional.

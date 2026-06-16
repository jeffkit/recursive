# Goal 296 — Replace Mutex<Instant> for last_active with AtomicU64

**Roadmap**: Post-Phase (invariant compliance + performance)

**Design principle check**:
- Implemented as: replace `Arc<std::sync::Mutex<Instant>>` in `SessionState.last_active`
  with `Arc<AtomicU64>` storing milliseconds since a fixed epoch.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`SessionState.last_active` is `Arc<std::sync::Mutex<std::time::Instant>>`.
Two problems:

1. **Invariant #5 violation**: `session.last_active.lock().unwrap()` appears
   in `src/http/handlers.rs:844` and `src/http/mod.rs:960`. The `.unwrap()`
   can panic if another thread panicked while holding the lock (mutex
   poisoning). Non-test code must never use `.unwrap()`.

2. **Unnecessary overhead**: `Mutex` involves an OS-level lock acquisition for
   a simple timestamp read/write. Since `Instant` is 64-bit on most platforms,
   we can replace the Mutex with an `AtomicU64` storing milliseconds elapsed
   since process startup, which is lock-free.

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — change `SessionState.last_active` field

From:
```rust
pub last_active: Arc<std::sync::Mutex<std::time::Instant>>,
```

To:
```rust
/// Milliseconds since [`SESSION_EPOCH`] when this session was last active.
/// Updated atomically on every message. Used by the session reaper.
pub last_active_ms: Arc<AtomicU64>,
```

Add a module-level constant:
```rust
/// Reference instant for session last_active timestamps.
/// Stored as a `OnceLock` so it's computed once at startup.
static SESSION_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn session_epoch() -> std::time::Instant {
    *SESSION_EPOCH.get_or_init(std::time::Instant::now)
}

/// Read the current session timestamp as milliseconds since SESSION_EPOCH.
pub fn now_session_ms() -> u64 {
    session_epoch().elapsed().as_millis() as u64
}
```

### 2. `src/http/handlers.rs` — update all construction and update sites

Replace all `Arc::new(std::sync::Mutex::new(std::time::Instant::now()))` with
`Arc::new(AtomicU64::new(now_session_ms()))`.

Replace `*session.last_active.lock().unwrap() = std::time::Instant::now()` with
`session.last_active_ms.store(now_session_ms(), Ordering::Relaxed)`.

### 3. `src/http/mod.rs` — update the session reaper

Replace:
```rust
if now.saturating_duration_since(*session.last_active.lock().unwrap()) >= ttl {
```
With:
```rust
let last_ms = session.last_active_ms.load(Ordering::Relaxed);
let elapsed = std::time::Duration::from_millis(now_session_ms() - last_ms);
if elapsed >= ttl {
```

(Remove the `now` variable if it was used only for this calculation, or
adapt as needed.)

### 4. Tests

Existing tests should pass. Update any test that constructs `SessionState`
directly (check `tests/http.rs` and `tests/agui_e2e.rs`) to use
`last_active_ms: Arc::new(AtomicU64::new(0))` instead of the Mutex.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- No `Mutex<Instant>` in `SessionState`
- No `.lock().unwrap()` on `last_active` anywhere in production code
- Session reaper still evicts sessions past the TTL

## Notes for the agent

- Read `src/http/mod.rs` (SessionState, session_reaper, AppState) first.
- Read `src/http/handlers.rs` to find all `last_active` creation/update sites.
  There may be 3-5 sites (create_session, fork_session, send_session_message,
  test helpers).
- Use `Ordering::Relaxed` for both store and load — last_active is a
  best-effort timestamp, not a synchronization primitive.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.

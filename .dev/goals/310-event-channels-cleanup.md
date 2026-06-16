# Goal 310 — Clean up event_channels when sessions are evicted or deleted

**Roadmap**: Post-Phase (Memory leak fix)

**Design principle check**:
- Implemented as: removing the `event_channels` entry in `session_reaper`
  and `delete_session` when sessions are removed from `state.sessions`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`AppState.event_channels` (`Arc<RwLock<HashMap<String, broadcast::Sender<SseEvent>>>>`)
is populated when `send_session_message` or `session_events` is called for a
session. However, entries are **never removed** from this map when sessions
are evicted (by `session_reaper`) or explicitly deleted (by `delete_session`).

This is a **memory leak**: for a long-running server with many sessions, the
`event_channels` map grows indefinitely, consuming memory proportional to
the total number of sessions ever created (not just active ones).

The `broadcast::Sender<SseEvent>` itself is cheap (no subscribers after the
session is gone), but the `HashMap` entry and the `String` key remain
allocated forever.

**Affected code paths:**

1. **`session_reaper`** (in `src/http/mod.rs`): removes sessions from
   `state.sessions` but does not remove from `state.event_channels`

2. **`delete_session`** (in `src/http/handlers.rs`): removes from
   `state.sessions` but does not remove from `state.event_channels`

## Scope (do exactly this, no more)

### 1. `src/http/mod.rs` — fix `session_reaper`

In the eviction loop (Phase 2), after removing the session from `sessions`,
also remove it from `event_channels`:

```rust
// Phase 2: evict under a write lock, calling close() on each.
{
    let mut sessions = state.sessions.write().await;
    for id in &to_evict {
        if let Some(session) = sessions.remove(id) {
            if let Ok(mut rt) = session.runtime.try_lock() {
                rt.close(None).await;
            }
            state.metrics.sessions_active.fetch_sub(1, Ordering::Relaxed);
            tracing::info!("reaper: evicted idle session {id}");
        }
    }
}
// Prune stale event_channels for evicted sessions.
{
    let mut channels = state.event_channels.write().await;
    for id in &to_evict {
        channels.remove(id);
    }
}
```

### 2. `src/http/handlers.rs` — fix `delete_session`

In `delete_session`, after removing the session from `state.sessions`, also
remove it from `state.event_channels`:

```rust
state.sessions.write().await.remove(&id);
state.metrics.sessions_active.fetch_sub(1, Ordering::Relaxed);
// Clean up SSE event channel for this session.
state.event_channels.write().await.remove(&id);
```

### 3. Tests

Add a test that:
1. Creates a session
2. Sends a message (which creates an event channel entry)
3. Verifies the event channel entry exists
4. Deletes the session
5. Verifies the event channel entry is removed

```rust
#[tokio::test]
async fn delete_session_cleans_up_event_channels() {
    // ... setup ...
    assert!(state.event_channels.read().await.contains_key(&session_id));
    // delete session
    let res = client.delete(&format!("{base}/sessions/{id}")).send().await.unwrap();
    assert_eq!(res.status(), 204);
    assert!(!state.event_channels.read().await.contains_key(&session_id));
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- After session deletion or reaper eviction, the `event_channels` entry
  for that session is removed
- `state.event_channels.len()` equals `state.sessions.len()` for sessions
  that have been used

## Notes for the agent

- Read `src/http/mod.rs` around lines 1026–1050 for `session_reaper` Phase 2.
- Read `src/http/handlers.rs` around lines 425–450 for `delete_session`.
- The cleanup must happen AFTER the session is removed from `sessions` so that
  concurrent `session_events` calls that have already subscribed see the
  broadcast channel drop and close their streams gracefully.
- If you add a separate lock acquisition for `event_channels` (separate
  from the `sessions` write lock), ensure it does NOT happen while the
  `sessions` write lock is still held (to avoid potential lock-order deadlocks).
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`,
  or any non-HTTP files.

# Goal 280 — session_clear_goal returns 409 when force-clear fails

**Roadmap**: Phase 17 (Production Hardening) — P1 from
`docs/review/architecture-review-2026-06-15.md` (NEW-HTTP-15)

**Design principle check**:
- Implemented as: replace the best-effort retry loop in
  `runtime_goal_state_clear` with an explicit `409 Conflict`
  return path when the runtime is still busy after a short
  retry window.
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT add a new feature flag

## Why

`src/http/handlers.rs:656-682` (`session_clear_goal`):

```rust
match session.runtime.try_lock() {
    Ok(runtime) => runtime.clear_goal().await,
    Err(_) => {
        // Runtime is busy; force-clear via the shared goal_state.
        let _ = runtime_goal_state_clear(&session.runtime).await;
    }
}

(StatusCode::OK, Json(... {"status": "cleared"}))
```

If the runtime Mutex is held by a long turn (e.g. a
`max_steps=32` agent doing real work — easily 60-180s), the
inner `runtime_goal_state_clear` retries 5 times × 50ms = 250ms
then gives up. The handler still returns 200 OK with
`{"status": "cleared"}` — the client believes the goal is
cleared. It is not.

The retry limit was tuned for short tool calls, not for full
turn budgets. The fix: be honest with the client. Return 409 if
the runtime is still busy after the retry window, with a
`Retry-After: 5` header.

## Scope (do exactly this, no more)

### 1. Make `runtime_goal_state_clear` return success/failure

In `src/http/handlers.rs:684-694`:

```rust
async fn runtime_goal_state_clear(
    runtime: &Arc<tokio::sync::Mutex<crate::runtime::AgentRuntime>>,
) -> bool {
    // Try up to 10 times × 100ms = 1s window. If still busy, fail.
    for _ in 0..10u8 {
        if let Ok(rt) = runtime.try_lock() {
            rt.clear_goal().await;
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}
```

(Bumped from 5×50ms to 10×100ms — 1s total — to give short
turns a chance to drain without making the HTTP client wait
forever.)

### 2. Update `session_clear_goal` to return 409 on failure

In `src/http/handlers.rs:656-682`:

```rust
pub(super) async fn session_clear_goal(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> (StatusCode, [(&'static str, &'static str); 1], Json<serde_json::Value>) {
    let sessions = state.sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            [("retry-after", "0")],
            Json(serde_json::json!({"error": "session not found"})),
        );
    };

    match session.runtime.try_lock() {
        Ok(mut runtime) => {
            runtime.clear_goal().await;
            drop(runtime);  // release before async .write().await if any
            return (
                StatusCode::OK,
                [("retry-after", "0")],
                Json(serde_json::json!({"status": "cleared", "session_id": session_id})),
            );
        }
        Err(_) => {
            // Runtime is busy with an in-flight turn; retry briefly.
            if runtime_goal_state_clear(&session.runtime).await {
                return (
                    StatusCode::OK,
                    [("retry-after", "0")],
                    Json(serde_json::json!({"status": "cleared", "session_id": session_id})),
                );
            }
            return (
                StatusCode::CONFLICT,
                [("retry-after", "5")],
                Json(serde_json::json!({
                    "error": "session runtime is busy; goal not cleared",
                    "session_id": session_id,
                    "hint": "retry after the current turn completes"
                })),
            );
        }
    }
}
```

(Adjust the tuple return type to match axum's idiomatic
`(StatusCode, [(HeaderName, HeaderValue); N], Json<...>)` —
use a builder or `Response` if needed.)

### 3. Tests

In `src/http/handlers.rs` `#[cfg(test)]`:

```rust
#[tokio::test]
async fn clear_goal_returns_409_when_runtime_busy() {
    // Acquire the per-session runtime Mutex (simulating an
    // in-flight turn). Spawn the handler in a tokio task.
    // Assert the handler returns 409 and the Retry-After header
    // is "5".
    // Drop the lock, retry the handler, assert 200.
}
```

Per g268 test discipline: avoid spinning up a real Router; use
direct handler invocation with `axum::extract::State` and a
minimal AppState.

## Acceptance

- `cargo test --workspace` — green (existing + new test)
- `cargo clippy --all-targets --all-features -- -D warnings` —
  clean
- `cargo fmt --all` — applied
- `grep "StatusCode::CONFLICT" src/http/handlers.rs` — appears in
  `session_clear_goal` (in addition to existing 409s in other
  handlers)
- `grep "retry-after" src/http/handlers.rs` — 1+ match in the new
  path
- The new test passes; an existing clear_goal happy-path test
  still passes (regression)

## Notes for the agent

- The `(StatusCode, [headers], Json)` return type for axum 0.8
  may differ from earlier versions. Verify the axum version in
  Cargo.toml and use the matching shape. If axum's response
  builder is simpler, use that.
- Do NOT change the successful path's behavior — clients that
  work today must continue to see 200 + `{"status":"cleared"}`.
- The retry-after window (1s) is a tradeoff: too short and we
  reject legitimate short-turn clears; too long and the HTTP
  client times out. 1s is the middle.
- Estimated diff: 1 file (handlers.rs), ~40 lines net.
- **Test discipline reminder (from g268 post-mortem)**: the
  "runtime busy" simulation needs to acquire the per-session
  mutex *before* calling the handler — use `runtime.lock().await`
  in a `tokio::spawn` block, hold the guard across the handler
  invocation, then drop.

**Disjoint file guarantee**: This goal touches src/http/handlers.rs.
Goal 274 also touches src/http/handlers.rs but at different
methods (send_session_message vs session_clear_goal). Safe to
run in parallel *only* if the agent commits in disjoint hunks.
If serialized, goal 274 first, this goal second.
# Goal 237 — HTTP session lifecycle close + audit-on-poison fix

**Roadmap**: Arch-review bugfixes (part 1/3)

**Design principle check**:
- Implemented as: two targeted bug fixes, no new abstractions
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

Two silent data-loss bugs from the architecture review:

1. `delete_session` in `src/http/handlers.rs` drops the `SessionState`
   without calling `AgentRuntime::close()`, so `SessionEnd` hooks never
   fire for HTTP-hosted sessions (resource leaks, broken hook-based
   cleanup, `session_closed` flag never set).

2. `SessionPersistenceSink::emit()` in `src/session.rs` for the
   `MessageAppendedWithAudit` arm passes `None` on mutex-poison recovery
   (line ~1396) instead of forwarding the `audit` value. All subsequent
   tool-result messages silently lose their audit trail.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — `delete_session`

Before `sessions.remove(&id)`, acquire the session's runtime lock and
call `runtime.close(None).await`. Pattern:

```rust
pub(super) async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> StatusCode {
    let session = {
        let sessions = state.sessions.read().await;
        sessions.get(&id).cloned()
    };
    if let Some(s) = session {
        // fire SessionEnd before dropping
        let mut rt = s.runtime.lock().await;
        rt.close(None).await;
        drop(rt);
        state.sessions.write().await.remove(&id);
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
```

Also check: does `AgentRuntime::close` take `&mut self` or `&self`?
Read `src/runtime.rs` to confirm the signature before writing.

### 2. `src/session.rs` — `MessageAppendedWithAudit` arm

Change the poison-recovery path to forward `audit`:

```rust
Err(poisoned) => {
    let mut w = poisoned.into_inner();
    w.append_with_audit(&message, Some(audit), None, None)
}
```

Note: `audit` is moved into the match arm, so `Some(audit)` is valid.

### 3. Tests

In `tests/http.rs` or a new `#[cfg(test)] mod tests` block:
- Add a test that creates a session, deletes it, and verifies the
  session is gone (existing `delete_session` test if one exists, or add
  one). The important contract is that delete doesn't panic.

No new test for the poison-recovery path (mutex poisoning is hard to
trigger in tests; documenting the fix in the journal is sufficient).

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `delete_session` calls `runtime.close(None).await` before removing
  from the map
- `MessageAppendedWithAudit` poison-recovery passes `Some(audit)` not `None`

## Notes for the agent

- Read `src/http/handlers.rs` lines 368-378 (delete_session) and
  `src/session.rs` lines 1389-1403 (MessageAppendedWithAudit arm) first.
- Read `src/runtime.rs` `close()` signature to confirm argument type.
- `SessionState.runtime` is `Arc<tokio::sync::Mutex<AgentRuntime>>` —
  use `.lock().await` to acquire it.
- Use `apply_patch` / surgical edits only. Do NOT rewrite whole files.
- **DO NOT modify** `src/llm/`, `src/tools/`, `src/run_core.rs`,
  `src/compact.rs`, `src/config.rs`, or any test files unrelated to
  HTTP session lifecycle.

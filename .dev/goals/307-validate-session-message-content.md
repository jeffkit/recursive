# Goal 307 — Validate non-empty content in POST /sessions/:id/messages

**Roadmap**: Post-Phase (API input validation consistency)

**Design principle check**:
- Implemented as: adding an empty-content guard in `send_session_message`
  handler in `src/http/handlers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

`POST /run` validates that `goal` is non-empty before proceeding:
```rust
if body.goal.trim().is_empty() {
    return Err(ApiError::bad_request("missing or empty 'goal' field"));
}
```

But `POST /sessions/:id/messages` (`send_session_message`) sends the message
content directly to the runtime without any validation:
```rust
let run_result = runtime.enqueue(&body.content).await...
```

Sending an empty string to `runtime.enqueue()` works (it doesn't panic), but:
1. The agent receives an empty user turn, which produces unhelpful behavior
2. The error path returns a `(StatusCode, Json<ErrorResponse>)` instead of `ApiError`
   (G304 will fix the error type, but validation should also be added)
3. Consistent with `POST /run`, this handler should reject empty content upfront

Note: This goal only adds empty-content validation. The `ApiError` migration
for this handler is handled by G304.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — add guard in `send_session_message`

After reading the session (around line 855 where the session lookup happens),
add an early validation of `body.content` before acquiring the semaphore:

```rust
// Validate: message content must not be blank.
if body.content.trim().is_empty() {
    return Err((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            status: "error".into(),
            error: "message content must not be empty".into(),
        }),
    ));
}
```

Place this immediately after the session lookup result is destructured
(after the line that gets `runtime_arc, interrupt_token_arc, ...`), so
we validate before acquiring the semaphore (which has a capacity limit).

### 2. Tests

Add a test in `tests/http.rs` (or the `#[cfg(test)]` section) that:
1. Creates a session
2. Sends `POST /sessions/:id/messages` with `{"content": ""}` (empty)
3. Expects a 400 status code response

```rust
// Example test structure (adapt to match actual test infrastructure):
#[tokio::test]
async fn send_session_message_rejects_empty_content() {
    // setup...
    let res = client
        .post(&format!("{base}/sessions/{id}/messages"))
        .header("x-api-key", "test-key")
        .json(&serde_json::json!({"content": ""}))
        .send().await.unwrap();
    assert_eq!(res.status(), 400);
}
```

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- `POST /sessions/:id/messages` with empty `content` returns 400
- `POST /sessions/:id/messages` with whitespace-only `content` returns 400
- `POST /sessions/:id/messages` with non-empty `content` still works

## Notes for the agent

- Read `src/http/handlers.rs` around lines 828–950 for the full
  `send_session_message` function.
- The guard should be added before the `state.run_semaphore.acquire_owned()`
  call (around line 873) to avoid consuming semaphore capacity for invalid
  requests.
- The error type for this handler is currently `(StatusCode, Json<ErrorResponse>)`
  — use the same pattern as the existing error returns in this function
  (not `ApiError`) since the full migration to `ApiError` is handled by G304.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/http/mod.rs`,
  or any non-HTTP files.

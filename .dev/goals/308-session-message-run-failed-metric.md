# Goal 308 — Track agent_runs_failed for failed POST /sessions/:id/messages

**Roadmap**: Post-Phase (Metrics completeness)

**Design principle check**:
- Implemented as: adding `record_run_failed()` call in the error path of
  `send_session_message` in `src/http/handlers.rs`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

In `send_session_message` (`POST /sessions/:id/messages`), when the agent
run fails (i.e., `runtime.enqueue()` returns `Err(...)`), the handler returns
a 500 error but does NOT call `record_run_failed()`.

This means:
- `recursive_agent_runs_total` counter is **not** incremented on failure
- `recursive_agent_runs_failed` counter is **not** incremented on failure
- The Prometheus `/metrics` endpoint undercounts total runs and missed errors

The error code path (lines ~966–975 in handlers.rs):
```rust
let outcome = run_result.map_err(|e| {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            status: "error".into(),
            error: format!("agent run failed: {e}"),
        }),
    )
})?;
```

The `?` returns early without calling `record_run_failed(&state.metrics)`.

Compare with `run_agent` (line ~130) which correctly calls
`record_run_failed(&state.metrics)` before returning an error response.

## Scope (do exactly this, no more)

### 1. `src/http/handlers.rs` — add `record_run_failed` call

Change:
```rust
let outcome = run_result.map_err(|e| {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            status: "error".into(),
            error: format!("agent run failed: {e}"),
        }),
    )
})?;
```

To:
```rust
let outcome = run_result.map_err(|e| {
    record_run_failed(&state.metrics);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            status: "error".into(),
            error: format!("agent run failed: {e}"),
        }),
    )
})?;
```

### 2. Tests

Add a test in `tests/http.rs` (or `src/http/handlers.rs` `#[cfg(test)]`) that:
1. Creates a session with a mock provider that always fails
2. Sends a message to the session
3. Gets a 500 response
4. Checks that `recursive_agent_runs_total` and `recursive_agent_runs_failed`
   were both incremented in the metrics

If mocking a failing provider is complex, a simpler test that just verifies
the metric is initialized to 0 is acceptable as a baseline.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- When `POST /sessions/:id/messages` returns 500 (LLM error),
  `recursive_agent_runs_total` and `recursive_agent_runs_failed` are
  both incremented in `/metrics`

## Notes for the agent

- Read `src/http/handlers.rs` around lines 945–990 for the full context
  of the `run_result.map_err(...)` block.
- Read lines 30–55 to understand the `record_run_failed()` function signature.
- Read lines 120–145 to see how `run_agent` correctly calls `record_run_failed`.
- The fix is a one-line addition: `record_run_failed(&state.metrics);` inside
  the `map_err` closure before the tuple return.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/http/mod.rs`,
  or any non-HTTP files.

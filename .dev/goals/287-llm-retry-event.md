# Goal 287 — Emit AgentEvent::LlmRetry on LLM Backoff

**Roadmap**: Post-Phase (Observability) — Improvement 3/3 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: new `AgentEvent` variant + emission in `RunCore::call_llm_with_retry`
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

When the LLM returns a retryable error (rate limit / timeout), `RunCore::call_llm_with_retry`
backs off and retries silently. Callers (TUI, HTTP SDK consumers) have no visibility into
this: from their perspective the agent appears frozen during the wait.

Adding an `AgentEvent::LlmRetry` event lets:
- The TUI show "Rate limited, retrying in 2s…" 
- SDK consumers implement their own progress indicators
- Log analysis correlate long-latency turns with retry storms

## Scope (do exactly this, no more)

### 1. `src/event.rs`

Add a new variant to the `AgentEvent` enum:

```rust
/// Emitted when the LLM call fails with a retryable error (rate limit or timeout)
/// and the agent will back off before retrying.
LlmRetry {
    /// Which step (ReAct iteration) triggered the retry.
    step: usize,
    /// Which retry attempt this is (1 = first retry after initial failure).
    attempt: u32,
    /// How many milliseconds the agent will sleep before the next attempt.
    wait_ms: u64,
    /// Short human-readable description of the error ("rate_limited" or "timeout").
    reason: String,
},
```

If `AgentEvent` has a `#[non_exhaustive]` attribute or a match in tests,
update accordingly. Search for all `match event` / `AgentEvent::` patterns
and add the new arm (typically a no-op `_ => {}` or an explicit branch).

### 2. `src/run_core.rs`

In `call_llm_with_retry`, after the `warn!(...)` log line and before the
`tokio::time::sleep(...)` call, emit the new event:

```rust
warn!(
    step, attempt, wait_ms, error = %e,
    "llm retryable error — backing off"
);
// Goal-287: surface retry to event consumers (TUI, SDK).
self.emit(AgentEvent::LlmRetry {
    step,
    attempt,
    wait_ms,
    reason: match &e {
        crate::error::Error::RateLimited { .. } => "rate_limited".to_string(),
        _ => "timeout".to_string(),
    },
});
tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
```

Note: `call_llm_with_retry` takes `&self` (immutable). The `emit` method already
takes `&self` (it sends on an `Option<mpsc::UnboundedSender>`), so this is compatible.

### 3. Tests

Add a test in `src/run_core.rs` or `src/runtime.rs`:

Scenario: configure a `MockProvider` that fails with `Error::RateLimited { retry_after_ms: 1 }`
on the first call, then succeeds. Collect events via a `ChannelSink`. Verify:
- `AgentEvent::LlmRetry { attempt: 1, reason: "rate_limited", .. }` is emitted
- The agent ultimately succeeds (the second call returns a `Completion`)

Use `retry_after_ms: 1` to keep the test fast (1ms sleep).

If `MockProvider` doesn't support injecting errors yet, add a minimal mechanism:
a `MockProvider` can accept a `Vec<Result<Completion, crate::error::Error>>` (wrapped
in Result) instead of just `Vec<Completion>`. If `MockProvider` is in `src/llm/mock.rs`,
add a `new_with_results` constructor that takes `Vec<Result<Completion, Error>>`.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- New test verifies `LlmRetry` event is emitted before the retry sleep
- All existing event-match patterns compile without warnings (add `LlmRetry { .. } => {}`)

## Notes for the agent

- `src/event.rs` and `src/run_core.rs` are the only files that MUST change.
- If `AgentEvent` is matched exhaustively anywhere (grep for `match.*AgentEvent` or
  `AgentEvent::` in `src/tui/`, `src/http/`, `tests/`), add the new arm there too.
- The `emit()` method on `RunCore` is: `fn emit(&self, event: AgentEvent)` — already `&self`, safe to call from `&self` context.
- `call_llm_with_retry` signature: `async fn call_llm_with_retry(&self, specs, stream_sender, step) -> Result<Completion>`
  — `step: usize` is already available, use it directly.
- **DO NOT modify** `src/kernel.rs`, `src/runtime.rs`, or provider files.

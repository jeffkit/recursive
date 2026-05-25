# Goal: transient-error retry in the OpenAI provider

## Motivation

In an earlier session a remote provider timed out mid-conversation and the
whole agent run died. Single network blips shouldn't kill a multi-minute
agent task. Retry transient failures a small number of times with backoff;
let permanent failures (4xx, JSON parse errors, etc.) surface immediately
as they do today.

## Requirements

### 1. Add a `RetryPolicy` type

In `src/llm/openai.rs`, add:

```rust
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: usize,
    pub initial_backoff: std::time::Duration,
    pub max_backoff: std::time::Duration,
}
```

with a `Default` impl: `max_retries = 2`, `initial_backoff = 1s`,
`max_backoff = 8s`.

Also add a **pure** method that's easy to test:

```rust
impl RetryPolicy {
    /// Decide whether the caller should wait and try again.
    /// `attempt` is 0-indexed (0 = the first retry decision after the
    /// initial try has failed). Returns `Some(backoff)` to retry,
    /// `None` to give up and propagate the error.
    pub fn backoff_for(
        &self,
        attempt: usize,
        status: Option<u16>,
        is_network_error: bool,
    ) -> Option<std::time::Duration> { /* ... */ }
}
```

Semantics:
- If `attempt >= max_retries`, return `None`.
- Otherwise, a failure is transient iff `is_network_error == true` OR
  `status.is_some_and(|s| (500..600).contains(&s))`.
- Non-transient → `None`.
- Transient → `Some(d)` where `d = (initial_backoff * 2^attempt)` clamped
  to `max_backoff`.

### 2. Wire the policy into `OpenAiProvider`

- `OpenAiProvider` gains a `retry: RetryPolicy` field (defaulted).
- Add a builder `with_retry_policy(RetryPolicy) -> Self`.
- The `complete()` method retries according to the policy:
  - On `reqwest::Error` from `.send()` → treat as network error (set
    `is_network_error = true`, `status = None`).
  - On HTTP 5xx response → consume the body for logging, then ask the
    policy; do NOT retry 4xx.
  - On JSON parse error of a 2xx body → DO NOT retry (likely a permanent
    protocol mismatch).
- Between attempts, `tokio::time::sleep(backoff).await`.
- Each retry emits `tracing::warn!` with `attempt`, `backoff`, and the
  reason (status or error string). Use `target: "recursive::llm"`.

### 3. Tests

Add **only unit tests** in the existing `#[cfg(test)] mod tests` at the
bottom of `src/llm/openai.rs`. Cover at minimum:

- `policy_retries_5xx_with_exponential_backoff`: assert
  `backoff_for(0, Some(503), false) == Some(1s)`,
  `backoff_for(1, Some(500), false) == Some(2s)`,
  `backoff_for(2, Some(500), false) == None` (default cap is 2 retries).
- `policy_retries_network_errors`: assert
  `backoff_for(0, None, true) == Some(1s)`.
- `policy_does_not_retry_4xx`: assert
  `backoff_for(0, Some(400), false) == None` and same for 401, 404, 429.
- `policy_caps_backoff_at_max`: build a `RetryPolicy` with `max_retries
  = 10`, `initial_backoff = 1s`, `max_backoff = 3s`; assert
  `backoff_for(5, Some(500), false) == Some(3s)` (would otherwise be 32s).

You do NOT need to integration-test the actual HTTP retry. The pure policy
+ careful wiring is enough; we trust the manual review for the integration
side.

## Out of scope

- Don't touch `LlmProvider` trait, `MockProvider`, `Agent`, tools, or CLI.
- Don't add dependencies.
- Don't touch `.dev/`.
- Don't change existing default behavior for callers that don't set a
  custom policy — they get the new defaults transparently.

## Definition of done

- `cargo build` and `cargo test` green.
- All existing tests in `src/llm/openai.rs` still pass unchanged.
- 4 new unit tests added and passing.
- `OpenAiProvider::new(...)` continues to work without changes
  (default `RetryPolicy` applied automatically).

## Final summary

List the files touched (`src/llm/openai.rs` only), the new public surface
(`RetryPolicy`, `with_retry_policy`), and the test result line.

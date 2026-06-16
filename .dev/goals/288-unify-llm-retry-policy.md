# Goal 288 — Unify LLM retry into RetryPolicy, remove duplicate constant

**Roadmap**: Post-Phase (Arch-review cleanup) — C4 from arch-review 2026-06-16

**Design principle check**:
- Implemented as: remove `LLM_MAX_RETRIES` constant from `run_core.rs`,
  drive retries exclusively via `RetryPolicy` already wired through the provider
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why

There are two independent retry layers for LLM calls:

1. **Provider-level** `RetryPolicy` in `src/llm/mod.rs` — `max_retries`,
   `initial_backoff`, `max_backoff` — wired per-provider in `src/cli/builder.rs`.
2. **Agent-loop-level** `LLM_MAX_RETRIES = 3` constant + `call_llm_with_retry`
   in `src/run_core.rs` — a second retry loop that wraps the provider call.

When both fire, a transient error can trigger up to `LLM_MAX_RETRIES ×
RetryPolicy.max_retries` attempts. Operators configuring `RetryPolicy` via
config don't know about the second layer. The `AgentEvent::LlmRetry` event
(added in g287) only reports the outer loop — the inner provider retries
are invisible.

## Scope (do exactly this, no more)

### 1. `src/run_core.rs` — remove `call_llm_with_retry` wrapper

Read `call_llm_with_retry` and its callers in `run_core.rs`.

Remove (or inline) the `LLM_MAX_RETRIES` constant and the manual retry loop
in `call_llm_with_retry`. Replace the call with a single direct call to the
provider (the provider's `RetryPolicy` handles retries internally).

Keep the `AgentEvent::LlmRetry` event emission — but move it into the
provider layer OR remove it (since provider retries are not directly
observable from `run_core.rs` once the double-layer is collapsed).

If removing `AgentEvent::LlmRetry` would be a regression (g287 added it
for observability), keep the event but emit it via a provider-level hook
instead. **Simplest acceptable outcome**: remove the outer retry loop so
only `RetryPolicy` retries fire.

### 2. Widen `RetryPolicy` defaults if needed

Check `src/cli/builder.rs` for the default `RetryPolicy`:

```rust
RetryPolicy {
    max_retries: config.retry_max,       // default from config
    initial_backoff: ...,
    max_backoff: ...,
}
```

If `config.retry_max` defaults to 0 (no retries), bump the default to 3 to
preserve existing retry behavior. Document the default in `src/config.rs`.

### 3. Tests

Existing tests should still pass. No new tests needed unless removing the
outer loop changes observable behavior that existing tests assert on.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo fmt --all` clean
- Only one retry layer exists (provider `RetryPolicy`)
- No `LLM_MAX_RETRIES` constant in `run_core.rs`

## Notes for the agent

- Read `src/run_core.rs` `call_llm_with_retry` and `LLM_MAX_RETRIES` first.
- Read `src/llm/mod.rs` `RetryPolicy` and how it's used in `call_with_retry`.
- Read `src/cli/builder.rs` to see default `RetryPolicy` values from config.
- Read `src/config.rs` for `retry_max` default.
- **DO NOT modify** `src/agent.rs`, `src/runtime.rs`, `src/kernel.rs`.
- **DO NOT call `exit_plan_mode` or `request_plan_mode`.** Headless run.

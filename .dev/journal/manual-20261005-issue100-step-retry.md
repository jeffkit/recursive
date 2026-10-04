# Issue #100 — cross-step retry for transient provider failures

Date: 2026-10-05
Goal: #100 `fix(llm): provider 重试默认仅 2 次、耗尽后 Err 直接终结 turn——无跨 step 退避，长任务对网关抖动零容忍`

## Problem

When a provider's own `RetryPolicy` budget was spent, the error bubbled out of
`RunCore::run_inner` and ended the whole turn — discarding every step already
completed in a long task for what is often seconds of gateway throttling
(429 / 5xx / network). Goal 288 had removed the earlier outer loop precisely
because it was an unbounded-ish duplicate; what was missing was a *bounded,
cancel-aware* second layer.

## Change

Re-introduce a bounded cross-step retry at the ReAct step level, cancel-aware,
reusing `RetryPolicy`:

- `src/error.rs`: `Error::http_status()` (parses the adapters'
  `"HTTP <status>: …"` `Error::Llm` message, plus the structured
  `RateLimited => 429`), `Error::is_network_error()`, and
  `Error::is_transient_provider_error()` (429 / 5xx / network only — other
  4xx, config and tool errors are permanent). Network detection on
  `Error::Llm` is prefix-only (`"request failed:"` / `"SSE stream read
  error:"`) so a parse failure embedding the raw body cannot false-positive.
- `src/llm/pricing.rs`: `RetryPolicy::for_step_loop()` (default 3 retries,
  1s→30s — more patient than the provider default of 2, 1s→8s),
  `RetryPolicy::for_step_loop_from_env()` (env overrides), and
  `RetryPolicy::backoff_for_error()` (error → status/network → backoff).
- `src/kernel.rs` / `src/runtime/builder.rs`: `step_retry` plumbed from the
  kernel builder (default `for_step_loop_from_env`) and overridable via
  `AgentRuntimeBuilder::step_retry`.
- `src/run_core.rs`: `RunCore::step_retry` field,
  `dispatch_llm_step_with_retry` (re-issues the step's LLM call after a
  backoff, emits the existing `AgentEvent::LlmRetry`, stops on permanent
  error / budget exhaustion), and `sleep_cancel_aware` (the backoff sleep
  now honours the shutdown token — the pre-Goal-288 loop used a plain
  `tokio::time::sleep`). `run_inner` keeps its 144-line body (≤150 invariant).

Config knobs are env-only (documented in `.env.example`), mirroring the
`RECURSIVE_HARD_STEP_CAP` precedent: `RECURSIVE_STEP_RETRY_MAX` (default 3),
`RECURSIVE_STEP_RETRY_INITIAL_BACKOFF_SECS` (1),
`RECURSIVE_STEP_RETRY_MAX_BACKOFF_SECS` (30). No `Config` field (`Config`
literals are exhaustive in 18 places); `for_step_loop_from_env` reads the
process env once at kernel build. The modest default keeps the
"unreachable API base must fail fast" contract of
`build_runtime_threads_per_turn_slot_into_agent_tool` intact (7 s of step
backoff, well under its 20 s budget); operators with longer throttling
windows raise the cap.

## Tests

- `src/error.rs`: status parsing, network classification, transient-vs-permanent.
- `src/llm/pricing.rs`: step policy is more patient than the provider default;
  env overrides (single test — env is process-global); `backoff_for_error`
  accepts transient and rejects permanent/exhausted.
- `src/run_core.rs`: recover from a 429; give up after the budget with the
  last transient error; a 400 is not retried (no second LLM call); the
  backoff sleep aborts on cancellation instead of parking 30s.
- `src/runtime/tests.rs`: `llm_retry_emits_event` → `llm_retry_recovers_and_emits_event`
  (the injected 429 now recovers and an `LlmRetry` event is emitted).
- `tests/http.rs`: `run_returns_429_with_retry_after_on_rate_limited` →
  `run_recovers_from_transient_rate_limit` (the /run request now succeeds);
  the 429 → `Retry-After` mapping is pinned by a new `map_run_error` unit
  test in `src/http/handlers.rs` instead.

## Notes

No new dependencies. No new tool/provider. `run_inner` unchanged in size.

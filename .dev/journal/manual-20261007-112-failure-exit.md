# Manual change — issue #112

- **Date:** 2026-10-07
- **Goal:** #112 fix(cli): 失败出口主动误导机器消费者——`--output-format json`
  全零值信封、`resume` 连 result 行都不发、错误里无 trace id
- **Depends on:** #110 (session finalize on error) + #115 (failed-turn usage) —
  both already landed at the baseline; this change is what remained.

## Files touched

### 1. Failed runs now report the work that actually happened

- `src/kernel.rs`
  - `FailureUsage = Arc<Mutex<TokenUsage>>` → `FailureOutcomeSlot =
    Arc<Mutex<FailureOutcome>>`, where `FailureOutcome { usage, steps,
    llm_latency_ms }` is the partial outcome of a turn that ended in `Err`.
  - `TurnContext::failure_usage` → `failure_outcome`.
- `src/run_core.rs`
  - `fail_step` publishes the full partial outcome (`total_usage` +
    `compaction_usage`, the failing `step`, `total_llm_latency_ms`) instead of
    usage alone.
- `src/runtime.rs`
  - `last_failed_usage: TokenUsage` → `last_failed: FailureOutcome`; the
    accessor `last_failed_usage()` is unchanged and a new
    `last_failed_outcome()` exposes the steps/latency. Worker spend (#119) is
    still folded into `usage`.
- `src/runtime/builder.rs`, `src/multi.rs`, `src/kernel/tests.rs`,
  `tests/agent_team_integration.rs` — field/type renames.
- `crates/recursive-cli/src/main.rs`
  - `run_once`'s `Err` arm reads `last_failed_outcome()` and forwards
    `steps` / `llm_latency_ms` (and usage) to `JsonEventTask::finish`. The
    terminal envelope used to be built from a Single-mode emitter that never
    sees events, so `num_turns` / `duration_api_ms` / `total_cost_usd` were
    hard zeros; now they carry the failed turn's real numbers.

### 2. `resume` emits a terminal `result` on failure

- `crates/recursive-cli/src/cli/resume.rs`
  - Both error paths (the primary `runtime.run(...)` and the
    `accept_user_messages` mid-turn loop) now drop the runtime, emit the
    terminal Claude `result` envelope through the printer (mirroring
    `run_once`), *then* finalize the session/cost and return. The old `?`
    returned before `task.finish`, so a stream-json consumer waiting for
    `result` hung forever.

### 3. Errors carry provider + model + the provider's trace id

- `src/error.rs`
  - `Error::Llm { provider, message }` → `{ provider, model: Option<String>,
    message, request_id: Option<String> }`. `provider` now names the adapter,
    never the model. `Error::RateLimited` gained `request_id`.
  - Display is rendered by `format_llm_error` / `format_rate_limited`
    (`[request_id=…]` appended only when present).
  - New accessors `Error::request_id()` and `Error::llm_site()`.
- `src/llm/mod.rs`
  - Shared `request_id_from_headers` (`x-request-id` → `request-id` →
    `traceparent`) and `retry_after_ms` (`Retry-After` integer seconds → ms),
    plus `warn_retries_exhausted`.
- `src/llm/openai.rs`, `src/llm/anthropic.rs`
  - `make_err` attributes the adapter (`provider_name`) and model separately;
    `with_provider_name` lets the factory pass `config.provider_type`.
  - `post_json_with_retry` / `stream_inner` capture the request id +
    `Retry-After` **before** the body is consumed. When the retry budget runs
    out they now emit a WARN (previously the final give-up was silent) and, for
    a 429, return the structured `Error::RateLimited { provider,
    retry_after_ms, request_id }` instead of a generic LLM error — making the
    previously test-only variant live.
  - `process_sse_line` (OpenAI) takes provider + model so parse failures carry
    both.
- `src/llm/factory.rs` — both arms set `with_provider_name(config.provider_type)`.
- Mechanical migration of every `Error::Llm` / `Error::RateLimited` literal to
  the new shape (tests under `src/`, `tests/`, and `crates/recursive-cli/`).

## Tests added

- `crates/recursive-cli/tests/cli_resume_surfaces.rs`
  - `resume_emits_an_informative_result_when_the_provider_fails` — a failing
    provider under `--output-format json` must emit a `result` line with
    `is_error: true`, `num_turns: 1` (not 0), and a `provider_stop` reason.
- `src/error.rs`
  - `llm_error_surfaces_provider_model_and_request_id`,
    `llm_error_omits_absent_model_and_request_id`,
    `rate_limited_reports_request_id_and_provider`.
- `src/llm/mod.rs`
  - `request_id_prefers_x_request_id_then_request_id_then_traceparent`,
    `retry_after_parses_integer_seconds_only`.
- `src/llm/openai.rs`
  - `exhausted_429_becomes_rate_limited_with_request_id`,
    `exhausted_5xx_llm_error_carries_provider_model_and_trace_id`.
- `src/run_core.rs` — `run_inner_publishes_partial_usage_when_the_turn_errors`
  now also asserts `steps == 2`.
- `src/runtime/tests.rs` — `failed_turn_exposes_last_failed_usage` extended to
  assert `last_failed_outcome().steps == 2`.

## Notes

- The `--output-format json` (Single) mode keeps the emitter un-fed by design
  (`drain_events`), so correctness depends entirely on the caller passing the
  real outcome into `finish` — which is exactly what fix #1 does.
- 429 → `RateLimited` changes how an exhausted rate limit surfaces over HTTP
  (`map_run_error` maps `RateLimited` to 429 + `Retry-After`, which is the
  truthful status; a generic `Llm` error mapped to 500). `Retry-After` is only
  read as integer seconds — an HTTP-date yields 0 ("retry immediately").
- The trace id is the provider's own correlation id from the response headers;
  it is not a substitute for the local OTel/Langfuse trace (#124), it is what
  lets an operator join a failed run to the provider's logs.

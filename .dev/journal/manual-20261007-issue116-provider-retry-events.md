# Issue #116 — make provider-internal retries visible (LlmRetry was half-dead)

Date: 2026-10-07
Goal: #116 `fix(events): LlmRetry 是纵贯全链的死事件——重试/退避/429 在前台与出口全部不可见`

## Problem

`AgentEvent::LlmRetry` had consumers but no producer for the retry layer that
actually fires most often. Issue #100 had already added a *cross-step* retry
loop in `run_core` that emits the event live, but a provider still retried
transient failures **inside its own request loop** (`openai.rs::post_json_with_retry`
+ the streaming round; `anthropic.rs::post_with_retry` + the streaming round)
and reported them with a `tracing::warn!` only. Those retries — and their
backoff sleep — were invisible in the event stream, in the TUI, and in the OTel
metrics, and the sleep landed inside `Latency::llm_ms` (measured around
`call_llm`), so a 40 s step could not be told apart from "the model is slow".

## Change

Keep both retry layers (provider = fast per-request, step = patient
cross-step — the split #100 deliberately created); make the provider's layer
report what it did.

- `src/llm/pricing.rs`: new `RetryRecord { attempt, wait_ms, status, reason }`,
  `RetryLog` (an `Arc<Mutex<Vec<RetryRecord>>>` with `record`/`take`, cheap
  because only the retry path touches it), and `retry_status_reason(status)`
  (429 → `rate_limited`, other transient statuses → `server_error`).
- `src/llm/mod.rs`: `ChatProvider::take_retry_records()` (default empty), so
  `MockProvider` and any provider without an internal loop stay untouched.
- `src/llm/openai.rs` / `src/llm/anthropic.rs`: each retry site records into its
  `RetryLog` immediately before the backoff sleep — 5 sites in openai
  (empty-body / transient-HTTP / network × non-stream + stream), 4 in anthropic
  (transient-HTTP / network × non-stream + stream). Behaviour unchanged.
- `src/run_core.rs`: `dispatch_llm_step` drains `take_retry_records()` after the
  call and `emit_provider_retries` re-emits each as `LlmRetry` (with the
  step). The step-loop emission now also carries the HTTP status.
- `src/event.rs`: `LlmRetry` gains `status: Option<u16>` (`#[serde(default)]`
  keeps the wire format backward-compatible).
- Consumers: `claude_json.rs` fills the previously hard-coded
  `"error_status": null`; the TUI maps `LlmRetry` (it used to fall through to
  `_ => None`, with a test pinning that drop) to a new `UiEvent::LlmRetry` and
  renders a System note; `observability/collector.rs` carries the status on the
  `llm.retry` span event and adds per-class trace metadata
  (`llm_retries_rate_limited` / `_server_error` / `_timeout` / `_network` /
  `_empty_body`) so a 429 burst and a 5xx outage are distinguishable.

Known limitation (documented on `emit_provider_retries`): the log is
provider-scoped, not call-scoped — sub-agents share the parent's
`Arc<dyn ChatProvider>` — so under concurrent sub-agents a drain may hand back
a retry another run produced. Every retry is still emitted exactly once; only
the `step` attribution is best-effort under that sharing. Runtime visibility in
Langfuse is issue #124's job.

## Tests added

- `src/llm/pricing.rs`: 429-vs-5xx label split; `RetryLog` record/drain order +
  `attempt` 1-based numbering; `Clone` shares one queue.
- `src/llm/mock.rs`: `MockProvider::with_retry_records` + one-shot
  `take_retry_records` (the runtime test driver — an `impl ChatProvider`
  outside `src/llm/` would trip invariant #10).
- `src/llm/openai.rs` / `anthropic.rs`: the existing give-up tests now also
  assert the retry log (two records, attempts 1/2, status/reason) and that a
  drain clears it.
- `src/runtime/tests.rs`: `provider_internal_retries_are_emitted_as_llm_retry_events`
  — a provider that retried internally surfaces one `LlmRetry` with
  step/attempt/wait_ms/status/reason.
- `src/observability/collector.rs`: status rides on the `llm.retry` span event;
  root metadata splits the total by class (429 / 5xx / network / empty-body).
- `crates/recursive-cli`: `api_retry` frame carries `error_status` (429), and
  stays `null` when the retry had no status.
- `crates/recursive-tui`: map test for `LlmRetry`; sink-forward test; handler
  test asserting the System note; the old "unmapped event" sink test now uses
  `Microcompact` since `LlmRetry` is mapped.

## Verification

`cargo fmt --all`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
`cargo test --workspace`; `.dev/scripts/tui-test-presence.sh` — all clean.

## Notes

No new dependencies. No new tool/provider. `run_inner` untouched.

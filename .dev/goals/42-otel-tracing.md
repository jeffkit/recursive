# Goal 42 — OpenTelemetry Tracing (Phase 4.5)

> **Roadmap**: feature 4.5, S size, Low impact (but production-critical).
> **Design principle check**: orthogonal — adds tracing spans around
> existing operations without changing their behavior. Pluggable —
> opt-in via env var; default behavior unchanged. Testable — we can
> assert that the relevant spans are created using
> `tracing-test::traced_test`.

## What

Add structured `tracing` spans around the hot paths so an operator
can attach a tracing subscriber (stdout-fmt, otlp-exporter, etc.) and
get telemetry without any code changes. **We do NOT add an
otlp-exporter dependency** — that's a separate `recursive-otlp`
crate someday. This goal is purely the **instrumentation** layer.

Spans to add (each with relevant fields):

- `agent.run { goal: &str }` — top-level run span.
- `agent.step { step: u32 }` — per-step span.
- `llm.complete { provider, model, prompt_tokens?, completion_tokens? }`
  — exit-event fields populated after the call.
- `tool.execute { name, args_size }` — per-tool span.
- `compact.run { removed, kept, summary_chars }` — only when compactor
  fires; already a StepEvent.

We already use `tracing::info!`/`tracing::warn!` in places. The goal
is to convert ad-hoc log lines into spans with structured fields,
which downstream consumers (Honeycomb, Datadog, Jaeger) can pivot on.

## Why

Right now the only observability is the JSONL event stream
(`--json`) + per-step latency. Production users need:
- Spans for distributed tracing (correlation across worktrees).
- Counters/histograms for metric dashboards.
- Structured logs with consistent field names.

This is the smallest, cheapest instrumentation step that unlocks all
three.

## Tests

- `agent_run_creates_span` — use `tracing-test::traced_test`,
  assert a span named `agent.run` was created with field `goal`.
- `agent_step_spans_nested_under_run` — assert nesting.
- `llm_complete_records_token_fields` — MockProvider with TokenUsage;
  assert span exit fields are set.

Add `tracing-test = "0.2"` dev-dependency.

## Wiring

- `src/agent.rs`: add `#[tracing::instrument(skip(self), fields(goal))]`
  on `Agent::run`, similar on `step`.
- `src/llm/openai.rs`: wrap `complete` with span.
- `src/tools/mod.rs::execute_with_tools` (or wherever tool calls are
  dispatched): wrap with `tool.execute` span.
- No CLI change. Operators choose their own subscriber.

## Acceptance

- `cargo build` green.
- `cargo test` green; +3 new tests.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all -- --check` clean.
- Baseline (no subscriber attached) behavior identical to before
  this goal — same stdout/stderr, same JSON event stream.

## Out of scope (defer)

- OTLP exporter integration (a Cargo feature flag in a future goal).
- Metrics (counters/histograms) — spans first, metrics later.
- Per-tool sub-spans for big multi-step tools.

# Goal 24 — Per-step LLM latency tracking

## Why

We track token cost per run but not *time*. When a run rolls back at
the 50-step `BudgetExceeded` cap or gets `Stuck`, we can't tell from
the journal whether the model itself was slow, retries were slow, or
the agent burned wall-clock waiting on tools. Adding per-step LLM
latency closes that observability gap and pairs naturally with the
existing `TokenUsage` plumbing.

## Scope

Touches: `src/agent.rs` and `src/main.rs::print_usage` (plus tests).

1. In `src/agent.rs`:
   - Around the `provider.complete(...)` call inside the step loop,
     measure elapsed wall-clock with `std::time::Instant::now()` and
     `instant.elapsed()`.
   - Add a `total_llm_latency_ms: u64` field to `AgentOutcome`
     (saturating-add into it each step).
   - Emit a new variant `StepEvent::Latency { step: u32, llm_ms: u64 }`
     immediately after the LLM returns (before any subsequent
     `StepEvent::ToolCall`). Add the variant to the existing
     `#[serde(tag = "kind", rename_all = "snake_case")]` enum so
     `--json` users see `{"kind":"latency","step":N,"llm_ms":M}`.

2. In `src/main.rs::print_usage`:
   - After the existing token-cost block, add one line:
     ```
     llm latency: total=<T>ms avg=<A>ms over <N> steps
     ```
   - Skip the line entirely if `total_llm_latency_ms == 0` or
     `steps == 0` (defensive — keeps the no-op `MockProvider` tests
     stable).

3. Tests:
   - Add a test (or extend an existing agent test) that runs the
     mock provider through ≥2 steps and asserts
     `outcome.total_llm_latency_ms > 0` is **not** required (mock is
     instant) — instead assert the field exists and is `u64` (compile
     test) and that `StepEvent::Latency` is emitted at least once per
     LLM call. Use `MockProvider` with two scripted responses.

## Acceptance

- `cargo build` green.
- `cargo test` green (123 baseline + ≥1 new = 124+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- Running `cargo run -- --json --goal "say hi" --max-steps 1` against
  a real provider would emit a `{"kind":"latency",...}` event (no need
  to actually run this — the test covering MockProvider is sufficient).

## Notes for the agent

- The exact place to instrument is the call inside `Agent::run`'s step
  loop that does `let completion = self.provider.complete(...).await?;`.
  Wrap just that, not the surrounding tool-execution.
- `StepEvent` already derives `Serialize` / `Deserialize` from
  goal-14 — just add the new variant; don't restructure the enum.
- Don't add a separate "tool latency" metric in this goal — scope
  it to LLM only. Tool latency is a follow-up.
- Use `.to_string()` not `.into()` for string literals in tests
  (AGENTS.md section 5).

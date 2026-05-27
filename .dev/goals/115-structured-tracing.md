# Goal 115 — Tool execution spans + run completion event

**Roadmap**: Phase 15.1 — Observability (part 1/3)

**Design principle check**:
- Implemented as: tracing span around tool dispatch + info event at run end
- Builds on existing `tracing::info_span!("agent.step", step)` in agent.rs
- Does NOT change the run loop logic, only adds instrumentation

## Why

Each agent step already has a tracing span, but we can't see how long
individual tool calls take within a step, or get a structured summary
at run completion. Adding these two pieces gives the evaluation system
the timing data it needs.

## Scope (do exactly this, no more)

### 1. Wrap tool execution in a span

In `src/agent.rs`, find where tool calls are dispatched (the section
that iterates over `tool_calls` and calls `self.tools.call(...)`).
Wrap each tool call in a span:

```rust
let tool_span = tracing::info_span!("tool.exec", tool = %call.name);
let _guard = tool_span.enter();
// ... existing tool call code ...
// After the call returns, the span auto-closes with duration
```

This is ~3 lines of change per call site.

### 2. Emit structured event at run completion

At the end of `Agent::run()`, just before returning `Ok(outcome)`,
emit a tracing event with the run summary:

```rust
tracing::info!(
    steps = outcome.steps,
    tokens_in = outcome.total_usage.prompt_tokens,
    tokens_out = outcome.total_usage.completion_tokens,
    finish = %format!("{:?}", outcome.finish),
    llm_latency_ms = outcome.total_llm_latency_ms,
    "agent.run.complete"
);
```

### 3. Tests

- **Test A**: Agent run emits "agent.run.complete" event (use tracing-test)
- **Test B**: Tool execution creates "tool.exec" span (verify via logs)

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` clean.
- Running with `RUST_LOG=info` shows `tool.exec` spans with durations.
- Only `src/agent.rs` is modified.

## Notes for the agent

- `tracing` is already imported in agent.rs: `use tracing::{debug, info, warn};`
- Existing step spans are at line ~451: `tracing::info_span!("agent.step", step)`
- The tool dispatch loop is in the section after the LLM call returns.
  Search for `self.tools.call` or `tool_registry.call` to find it.
- `tracing-test` is in dev-dependencies. Use `#[traced_test]` for test assertions.
- Do NOT touch `src/llm/openai.rs` or any other file. Only `src/agent.rs`.
- This should be a ~15 line change. Keep it minimal.

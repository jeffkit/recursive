# Goal 115 — Structured Tracing with span events

**Roadmap**: Phase 15.1 — Observability (part 1/3)

**Design principle check**:
- Implemented as: `tracing` span events on LLM calls + tool executions
- Builds on existing `tracing::info_span!` in agent.rs (step spans)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop logic

## Why

The agent already has basic tracing spans (one per step), but lacks
structured timing data for the two slowest operations: LLM API calls
and tool executions. Adding span events with duration and metadata
enables the evaluation system to extract per-step latency breakdowns
from logs, and helps debug "where did 30 seconds go?" during runs.

## Scope (do exactly this, no more)

### 1. Add LLM call span in `src/llm/openai.rs`

Wrap the HTTP request in a tracing span that records:
- model name
- token counts (on exit)
- latency (automatic from span lifetime)

```rust
let _span = tracing::info_span!(
    "llm.call",
    model = %self.model,
    tokens_in = tracing::field::Empty,
    tokens_out = tracing::field::Empty,
).entered();
// ... after response ...
_span.record("tokens_in", usage.prompt_tokens);
_span.record("tokens_out", usage.completion_tokens);
```

Do the same for `src/llm/anthropic.rs` if the file has a similar
request path.

### 2. Add tool execution span in `src/agent.rs`

In the tool dispatch section (where tools are called), wrap each
tool call in a span:

```rust
let _tool_span = tracing::info_span!(
    "tool.exec",
    tool = %tool_name,
    status = tracing::field::Empty,
).entered();
// ... after execution ...
_tool_span.record("status", if result.is_error { "error" } else { "ok" });
```

### 3. Add a `tracing` event for run completion

At the end of `Agent::run`, emit a structured event with the summary:

```rust
tracing::info!(
    steps = outcome.steps,
    tokens_in = outcome.total_usage.prompt_tokens,
    tokens_out = outcome.total_usage.completion_tokens,
    finish = ?outcome.finish,
    llm_latency_ms = outcome.total_llm_latency_ms,
    "agent.run.complete"
);
```

### 4. Tests

- **Test A**: Agent run emits "agent.run.complete" event (use tracing-test)
- **Test B**: Tool execution spans are created (verify span name)
- **Test C**: LLM call spans record token counts

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- Running with `RUST_LOG=info` shows structured span output including
  `llm.call`, `tool.exec`, and `agent.run.complete`.

## Notes for the agent

- `tracing` and `tracing-test` are already in Cargo.toml dependencies.
- There are existing spans in agent.rs at line ~451: `tracing::info_span!("agent.step", step)`.
  Build on that pattern — don't restructure.
- The existing `RECURSIVE_TRACE_SPANS` env var (used by self-improve.sh)
  relies on stderr output. New spans integrate with the standard `tracing`
  subscriber, which is already configured in main.rs.
- Keep changes minimal in agent.rs. The tool dispatch loop is around
  line 480-520 — search for tool execution there.
- In openai.rs, the HTTP call is in the `complete` method. That's
  the right place for the LLM span.
- Do NOT add new dependencies. `tracing` is already available.
- Files to modify: `src/agent.rs`, `src/llm/openai.rs`, possibly
  `src/llm/anthropic.rs`.

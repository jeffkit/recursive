# Goal 52 — Anthropic Provider Streaming

**Roadmap**: follow-up — Anthropic Provider streaming support

**Design principle check**:
- Implemented as: override of `LlmProvider::stream()` in the existing
  `AnthropicProvider` adapter (`src/llm/anthropic.rs`).
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

The `AnthropicProvider` (goal-34) currently relies on the default `stream()`
fallback in `LlmProvider` which calls `complete()` and emits the full text
as one chunk. The Anthropic Messages API supports true SSE streaming via
`"stream": true`, returning `content_block_delta` events with incremental
text. Without this, users who set `--stream` with an Anthropic backend see
no benefit (they wait for the full response then get it all at once).

`OpenAiProvider` already implements real SSE streaming (goal-32). This goal
mirrors that pattern for Anthropic's wire format.

## Scope (do exactly this, no more)

### 1. `src/llm/anthropic.rs` — implement `stream()`

Override the trait method:
```rust
async fn stream(
    &self,
    messages: &[Message],
    tools: &[ToolSpec],
    stream_tx: Option<StreamSender>,
) -> Result<Completion>
```

Behavior:
1. Build the same request body as `complete()`, but add `"stream": true`.
2. Send the request with `reqwest` and get a streaming response.
3. Parse SSE events line by line:
   - `event: message_start` → extract `usage.input_tokens`
   - `event: content_block_delta` with `delta.type == "text_delta"` →
     accumulate `delta.text`, send to `stream_tx`
   - `event: content_block_delta` with `delta.type == "input_json_delta"` →
     accumulate tool call JSON
   - `event: content_block_start` with `type == "tool_use"` →
     start a new tool call (capture id + name)
   - `event: message_delta` → extract `stop_reason`, `usage.output_tokens`
   - `event: message_stop` → done
4. Assemble the final `Completion` from accumulated data (same shape as
   `complete()` returns).
5. If `stream_tx` is `None`, still use streaming internally for the
   connection but don't send deltas (or just fall back to `complete()`
   — your choice for simplicity).

### 2. Error handling

- If SSE parsing fails mid-stream, return a clear error with what was
  received so far.
- Respect the existing retry logic (streaming failures are retryable
  at the `complete` level — if the connection drops, the caller retries).

### 3. Tests

- Test: streaming request includes `"stream": true` in body
- Test: text deltas are accumulated into `completion.content`
- Test: tool_use blocks are assembled into `completion.tool_calls`
- Test: `stream_tx` receives incremental text chunks
- Test: SSE with `stop_reason: "end_turn"` produces correct Completion
- Use mock TCP server pattern (same as existing anthropic tests) — NOT
  real API calls.

**Important**: all tests that use TCP listeners MUST set explicit reqwest
timeouts (request: 2s, connect: 1s) per AGENTS.md section 5.

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Streaming works end-to-end when using Anthropic backend with `--stream`
- Non-streaming path (`complete()`) unchanged
- No new dependencies

## Notes for the agent

- Read `src/llm/openai.rs` lines around the `stream()` implementation for
  the SSE parsing pattern. It uses `response.bytes_stream()` and manually
  parses `data:` lines. Mirror that approach.
- Anthropic SSE format differs from OpenAI's:
  - Events have `event: <type>\ndata: <json>\n\n` structure
  - Important event types: `message_start`, `content_block_start`,
    `content_block_delta`, `message_delta`, `message_stop`
  - Tool calls come as `content_block_start` (type=tool_use, id, name)
    followed by `content_block_delta` (input_json_delta) chunks
- The existing `AnthropicProvider` code already handles tool-call response
  parsing in `complete()`. Reuse the `ToolCall` construction logic.
- Don't forget: all mock server tests need explicit reqwest timeouts.

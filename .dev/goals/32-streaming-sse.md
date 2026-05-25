# Goal 32 — Streaming SSE for OpenAI-compatible providers

**Roadmap**: 1.3 — Streaming Completions

**Design principle check**:
- Implemented as: **extension to `LlmProvider` trait** (new optional
  `stream` method with default fallback to `complete`) + **new
  StepEvent variant** `StepEvent::PartialToken`. The agent loop
  observes events but does not branch on streaming logic.
- ❌ Does NOT add new branches inside `agent.rs::Agent::run`'s
  decision flow. Streaming, when enabled, drives the same `complete`
  semantics under the hood; the agent doesn't know or care.

## Why

All competitors stream token-by-token. Today `OpenAiProvider::complete`
waits for the full response — feels unresponsive on large outputs and
prevents early cancellation.

Streaming also unblocks future UX work: progress bars, cancel-mid-
generation, partial-render to terminal.

## Scope

Touches: `src/llm/mod.rs` (extend trait), `src/llm/openai.rs` (SSE
parsing + impl), `src/agent.rs` (new StepEvent variant + emit hook
during streaming if enabled), `src/main.rs` (CLI `--stream` flag, opt-in).

1. In `src/llm/mod.rs`:
   - Extend `LlmProvider` with `async fn stream(&self, messages:
     &[Message], tools: &[ToolSpec]) -> Result<Completion>` with a
     default implementation that delegates to `complete`.
   - The streaming version is expected to deliver `PartialToken` chunks
     via a side channel (see below) and return the final `Completion`
     when done. Same shape as `complete` for the final return.

2. In `src/llm/openai.rs`:
   - Implement `stream` for `OpenAiProvider` using
     `text/event-stream` (`stream: true` in request body). Parse SSE
     line-by-line (`data: {...}\n\n`), extract `choices[0].delta.content`
     deltas, accumulate, and assemble into a `Completion` matching the
     non-streaming shape.
   - On each chunk parsed, if a channel sender is configured, push the
     delta. (Channel sender provided via a thread-local or context arg —
     pick whichever is mechanically simpler. A
     `tokio::sync::mpsc::UnboundedSender<String>` field on the provider
     works.)

3. In `src/agent.rs`:
   - Add `StepEvent::PartialToken { step: u32, text: String }` to the
     existing enum. Wire `--json` serialization to match
     `#[serde(tag = "kind", rename_all = "snake_case")]`.
   - When streaming is enabled (via builder setting), pipe deltas
     through the existing `events` channel as `PartialToken` events.

4. In `src/main.rs`:
   - Add `--stream` CLI flag (default false). When set, configure the
     provider to stream and rendering to print deltas live.
   - The non-`--stream` path is completely unchanged.

5. Tests:
   - Unit test in `openai.rs`: spawn a minimal `TcpListener`-backed
     mock SSE server that serves a canned 3-chunk response, assert
     `stream` returns a `Completion` whose `text` is the
     concatenation of the chunks. **Use explicit reqwest timeout +
     connect_timeout** (AGENTS.md section 5 lesson — don't reignite
     the goal-30 deadlock).
   - Smoke test: `stream` falls back to `complete` for providers not
     implementing it.

## Acceptance

- `cargo build` green.
- `cargo test` green (140 baseline + 2 new = 142+).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.
- `--stream` is **opt-in**. Default behavior is unchanged.

## Notes for the agent

- SSE parsing pitfalls: lines may be split mid-event by TCP, you must
  buffer until a blank line. The `tokio_util::codec::LinesCodec` or a
  manual buffer over `bytes_stream()` both work.
- **MANDATORY** for any reqwest client in tests:
  `.timeout(Duration::from_secs(2)).connect_timeout(Duration::from_secs(1))`.
  This is the goal-30 lesson — see AGENTS.md section 5.
- Don't change the `Completion` struct shape. The streamed path
  builds the same struct, just incrementally.
- The mock SSE server pattern: `std::net::TcpListener::bind("127.0.0.1:0")`,
  spawn a thread that accepts one connection and writes
  `HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: ...\n\ndata: ...\n\n`.
- Use `apply_patch`. `.to_string()` over `.into()` in tests.

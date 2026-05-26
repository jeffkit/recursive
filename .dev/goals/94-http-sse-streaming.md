# Goal 94 — HTTP API: SSE Event Streaming

**Roadmap**: Phase 12.4 — HTTP API (part 4/6)

**Design principle check**:
- Implemented as: new SSE endpoint in `src/http.rs` using axum's SSE support
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Uses existing `StepEvent` observer pattern — no core changes

## Why

Currently POST /run and POST /sessions/:id/messages block until the agent
finishes. For long-running tasks, clients need real-time progress. SSE
(Server-Sent Events) streams step events as they happen — tool calls,
token generation, completion — so clients can show live status.

## Scope (do exactly this, no more)

### 1. `src/http.rs` — add SSE streaming endpoint

**GET /sessions/:id/events** — SSE stream of session events

When a client connects to this endpoint while a session is running,
they receive events as the agent processes steps. If no run is active,
the connection stays open until a message is sent to the session.

For simplicity in this first implementation:
- The SSE endpoint streams events from the NEXT message sent to the session
- Use `tokio::sync::broadcast` channel per session to fan out events

### 2. Event format

Each SSE event has:
```
event: step
data: {"type": "tool_call", "name": "read_file", "step": 1}

event: step
data: {"type": "token", "content": "partial text..."}

event: done
data: {"finish_reason": "NoMoreToolCalls", "total_steps": 5}
```

Event types to stream:
- `tool_call` — when a tool is invoked (name + step number)
- `tool_result` — when a tool returns (name + success/error)
- `token` — partial streaming token (if streaming enabled)
- `done` — agent finished (with finish_reason + steps)
- `error` — if agent errors out

### 3. Wire events into session message handler

Modify `send_session_message` to:
1. Check if there's a broadcast channel for this session
2. If so, emit step events through it as agent runs
3. Use a simple `StepEvent` → SSE event mapping

### 4. AppState additions

```rust
pub struct AppState {
    // ... existing fields ...
    pub event_channels: Arc<RwLock<HashMap<String, broadcast::Sender<SseEvent>>>>,
}
```

### 5. Tests

- Test: GET /sessions/:id/events returns SSE content-type
- Test: sending a message emits events on the SSE stream
- Test: done event is sent when agent finishes

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- SSE endpoint sends properly formatted Server-Sent Events
- Events include tool_call and done types at minimum

## Notes for the agent

- Read `src/http.rs` for current session handling (send_session_message).
- Read `src/agent.rs` for `StepEvent` enum — it already emits events via the
  `event_sender: Option<mpsc::UnboundedSender<StepEvent>>` on AgentBuilder.
- Use axum's built-in SSE support: `axum::response::sse::{Event, Sse}`.
- Use `tokio::sync::broadcast` for fan-out to multiple SSE clients.
- The agent's `event_sender` field (if present) sends `StepEvent` during execution.
  Wire this: agent builder → mpsc → forward to broadcast channel.
- For testing SSE, you can collect the response body as a stream and check events.
  Or test the event mapping logic in isolation.
- **DO NOT modify `src/agent.rs`.**
- **DO NOT implement WebSocket — SSE only.**
- **Keep it simple: one broadcast channel per session, created lazily.**

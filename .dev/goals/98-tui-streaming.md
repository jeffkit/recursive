# Goal 98 — TUI: Streaming Output + Tool Call Indicators

**Roadmap**: Phase 11.2 — TUI (part 2/5)

**Design principle check**:
- Implemented as: extension to `crates/recursive-tui/src/main.rs`
- ❌ Does NOT modify `src/agent.rs` or the core library
- TUI connects to the HTTP API to send messages and receive SSE events

## Why

The TUI scaffold (g97) is a local-only input box. Now it needs to
connect to the running HTTP server and show real agent output: streaming
tool calls as they happen, and the final response.

## Scope (do exactly this, no more)

### 1. HTTP client integration

Add `reqwest` (with blocking or async) to the TUI crate. The TUI will:
- On startup, create a session via POST /sessions
- On Enter (user submits message):
  - Send POST /sessions/:id/messages in a background task
  - Subscribe to GET /sessions/:id/events (SSE) for real-time updates
  - Display events as they arrive

### 2. Message types in the UI

Extend the messages list to show different styled entries:
- `You: <text>` — user messages (white)
- `🔧 <tool_name>` — tool call indicator (yellow/dim)
- `✓ <tool_name>` — tool result success (green/dim)
- `✗ <tool_name>` — tool result error (red/dim)
- `Assistant: <text>` — final response (cyan)

### 3. Async architecture

```rust
// Use tokio channels to communicate between HTTP tasks and the UI loop
struct App {
    input: String,
    messages: Vec<StyledMessage>,
    should_quit: bool,
    session_id: Option<String>,
    base_url: String,
    event_rx: Option<mpsc::UnboundedReceiver<UiEvent>>,
}

enum UiEvent {
    ToolCall { name: String },
    ToolResult { name: String, success: bool },
    AssistantMessage { content: String },
    Error { message: String },
}

enum StyledMessage {
    User(String),
    Assistant(String),
    ToolCall(String),
    ToolResult { name: String, success: bool },
    System(String),
}
```

### 4. Connection flow

1. TUI starts → tries to connect to `http://127.0.0.1:3000/health`
2. If healthy → creates session → shows "Connected to Recursive agent"
3. If unhealthy → shows "Not connected. Start server with: recursive http"
4. User types message + Enter → spawns async task to send + stream events
5. Events arrive → display in messages panel

### 5. Tests

- Test: StyledMessage formatting produces correct strings
- Test: UiEvent mapping to StyledMessage works correctly
- Test: App state transitions on receiving events

## Acceptance

- `cargo build -p recursive-tui` compiles
- `cargo test --workspace` green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- TUI shows tool call indicators when connected to a running server

## Notes for the agent

- Read `crates/recursive-tui/src/main.rs` for the current scaffold.
- The TUI main loop needs to be async (tokio) to handle both keyboard
  events and incoming HTTP/SSE events.
- Use `reqwest` (already in workspace deps) for HTTP calls.
- For SSE parsing, you can use reqwest's streaming body + simple line parsing
  (SSE format is `event: <type>\ndata: <json>\n\n`).
- The main loop should `tokio::select!` between:
  - crossterm event (keyboard input)
  - channel receive (incoming UI events from HTTP tasks)
- Add `reqwest` to `crates/recursive-tui/Cargo.toml` deps.
- **DO NOT modify any file in `src/` (the core library).**
- **DO NOT implement the full agent locally — always go through HTTP API.**
- **If the server isn't running, the TUI should still work (just show "not connected").**

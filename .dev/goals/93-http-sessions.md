# Goal 93 — HTTP API: Sessions (create, message, list)

**Roadmap**: Phase 12.3 — HTTP API (part 3/6)

**Design principle check**:
- Implemented as: session management in `src/http.rs` (or new `src/http/sessions.rs`)
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- Uses AgentRunner for multi-turn; sessions are HTTP-layer state

## Why

POST /run is fire-and-forget: one goal → one execution → done. Real
usage needs multi-turn conversations: a client creates a session, sends
messages, and receives responses while maintaining transcript context.
This is the HTTP equivalent of the REPL's conversation loop.

## Scope (do exactly this, no more)

### 1. Session data model (in `src/http.rs`)

```rust
use std::collections::HashMap;
use tokio::sync::RwLock;

#[derive(Clone, Debug, serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: String,  // ISO 8601
    pub message_count: usize,
}

// Add to AppState:
pub sessions: Arc<RwLock<HashMap<String, SessionState>>>,

// Internal (not serialized to client):
struct SessionState {
    id: String,
    created_at: String,
    transcript: Vec<Message>,
}
```

### 2. Endpoints

**POST /sessions** — create a new session
- Request: `{ "system_prompt": "optional override" }`
- Response: `{ "id": "uuid", "created_at": "..." }`

**GET /sessions** — list active sessions
- Response: `[{ "id": "...", "created_at": "...", "message_count": N }, ...]`

**POST /sessions/:id/messages** — send a message to a session
- Request: `{ "content": "user message" }`
- Response: `{ "role": "assistant", "content": "...", "finish_reason": "...", "usage": {...} }`
- Internally: append user message to transcript, run agent, append assistant response

**GET /sessions/:id** — get session details
- Response: `{ "id": "...", "created_at": "...", "message_count": N, "messages": [...] }`

**DELETE /sessions/:id** — delete a session
- Response: 204 No Content

### 3. Session state management

- Sessions stored in `Arc<RwLock<HashMap<String, SessionState>>>` in AppState
- Session ID: use `blake3::hash(timestamp + counter).to_hex()[..16]` (already have blake3 dep)
- No persistence — sessions are in-memory only (persistence is a future goal)

### 4. Tests

In `tests/http.rs`:
- Test: POST /sessions creates session, returns id
- Test: GET /sessions lists created sessions
- Test: POST /sessions/:id/messages with mock returns assistant response
- Test: GET /sessions/:id returns session with messages
- Test: DELETE /sessions/:id removes session
- Test: POST /sessions/:nonexistent/messages returns 404

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Multi-turn: create session → send message → send another → get session shows both exchanges

## Notes for the agent

- Read `src/http.rs` for existing AppState, build_router, run_agent handler.
- Read `src/agent.rs` for how `Agent::set_transcript()` works to resume with context.
- Read `src/message.rs` for `Message` struct and `Role` enum.
- The multi-turn pattern: build agent with system prompt, set transcript to session's
  messages, call agent.run(new_message), then append the new messages to session state.
- Use `uuid` crate if convenient, OR just blake3 hash for IDs (blake3 already a dependency).
- For timestamp, use `chrono` if already in deps, otherwise basic format from std.
  Actually, check Cargo.toml — if no chrono, just use a simple counter-based ID and
  skip the ISO timestamp (use "" or "TODO") to avoid adding deps. Or use
  `std::time::SystemTime` formatted manually.
- **DO NOT modify `src/agent.rs` or any tool file.**
- **DO NOT add persistence (files, database). In-memory only.**
- **DO NOT add WebSocket or SSE streaming in this goal — that's g94.**

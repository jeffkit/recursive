# Goal 104 — Multi-Agent: Inter-Agent Messaging Bus

**Roadmap**: Phase 13.3 — Multi-Agent Framework (part 3/5)

**Design principle check**:
- Implemented as: message bus in `src/multi.rs`
- ❌ Does NOT modify `src/agent.rs`
- Agents communicate through the bus, not by sharing transcripts

## Why

Agents in a multi-agent system need to communicate: the planner sends
tasks to the coder, the coder reports results to the reviewer, etc.
A messaging bus provides structured, async communication between agents
without coupling their transcripts.

## Scope (do exactly this, no more)

### 1. Message types

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub from: String,       // sender role name
    pub to: String,         // recipient role name (or "broadcast")
    pub content: String,
    pub msg_type: MessageType,
    pub timestamp: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum MessageType {
    Task,           // assignment from planner to coder
    Result,         // completion report
    Question,       // request for clarification
    Feedback,       // review feedback
    Broadcast,      // info for all agents
}
```

### 2. MessageBus

```rust
/// Async message bus for inter-agent communication.
#[derive(Clone)]
pub struct MessageBus {
    messages: Arc<RwLock<Vec<AgentMessage>>>,
    subscribers: Arc<RwLock<HashMap<String, broadcast::Sender<AgentMessage>>>>,
}

impl MessageBus {
    pub fn new() -> Self { ... }

    /// Send a message to a specific agent role.
    pub async fn send(&self, msg: AgentMessage) { ... }

    /// Subscribe to messages for a given role.
    pub fn subscribe(&self, role: &str) -> broadcast::Receiver<AgentMessage> { ... }

    /// Get all messages for a role (inbox).
    pub async fn inbox(&self, role: &str) -> Vec<AgentMessage> { ... }

    /// Get all messages sent by a role (outbox).
    pub async fn outbox(&self, role: &str) -> Vec<AgentMessage> { ... }

    /// Get full message history.
    pub async fn history(&self) -> Vec<AgentMessage> { ... }

    /// Clear all messages.
    pub async fn clear(&self) { ... }
}
```

### 3. Integrate with AgentPool

Add `bus: MessageBus` to AgentPool:
- Initialize in `new()`
- Add `pub fn bus(&self) -> &MessageBus` accessor
- Add `pub async fn send_task(&self, from: &str, to: &str, content: &str)` convenience method
- Add `pub async fn send_result(&self, from: &str, to: &str, content: &str)` convenience

### 4. Tests

- Test: send + inbox retrieves messages for recipient
- Test: outbox retrieves messages from sender
- Test: broadcast messages appear in all inboxes
- Test: subscribe receives new messages
- Test: history returns all messages
- Test: clear empties the bus
- Test: AgentPool convenience methods work

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean

## Notes for the agent

- Read `src/multi.rs` for current AgentPool with SharedMemory.
- Use `tokio::sync::broadcast` for pub/sub (already used in http.rs for SSE).
- Message IDs: use blake3 hash like sessions, or simple counter.
- The bus stores ALL messages (append-only log) plus per-role broadcast channels.
- **DO NOT modify `src/agent.rs`.**
- **Keep it simple — the bus is a communication primitive, not a task scheduler.**

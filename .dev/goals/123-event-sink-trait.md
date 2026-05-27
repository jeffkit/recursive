# Goal 123 — EventSink trait + AgentEvent enum

**Roadmap**: Kernel Architecture Refactor — Phase 1a (new abstractions)

**Design principle check**:
- Implemented as: new `src/event.rs` module
- Pure addition — no existing code modified (except lib.rs re-export)
- Does NOT modify agent.rs, main.rs, http.rs, or any existing module

## Why

The current codebase uses three different event/streaming patterns:
- CLI/TUI: `mpsc::UnboundedSender<StepEvent>`
- HTTP: `broadcast::Sender<SseEvent>`
- Multi-Agent: no events exposed

A unified `EventSink` trait allows any consumer to observe agent activity
through a single interface. This is the foundation for the Kernel/Wrapper
architecture where the Kernel emits events through an injected sink rather
than owning a channel.

## Scope (do exactly this, no more)

### 1. Create `src/event.rs`

```rust
use serde::{Serialize, Deserialize};
use tokio::sync::{mpsc, broadcast};

/// Trait for receiving real-time events from the agent.
/// Implementations decide how to transport events (channel, broadcast, log, null).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// Unified event type — superset of current StepEvent + SseEvent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    // Kernel-level (emitted during a turn)
    StepStart { step: usize },
    ToolCall { name: String, id: String, arguments: serde_json::Value, step: usize },
    ToolResult { id: String, name: String, output: String, step: usize },
    AssistantText { text: String, step: usize },
    PartialToken { token: String },
    Latency { step: usize, llm_ms: u64 },
    Usage { prompt_tokens: usize, completion_tokens: usize },
    TurnFinished { reason: String, steps: usize },

    // Wrapper-level (emitted across turns)
    Compacted { original_msgs: usize, summary_chars: usize },
    BgJobStarted { job_id: String, command: String },
    BgJobFinished { job_id: String, exit_code: i32 },
    ScheduledWakeup { delay_secs: u64, prompt: String },
    CostUpdate { session_total_usd: f64 },
}

/// Sends events through an mpsc unbounded channel.
pub struct ChannelSink {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

/// Sends events through a broadcast channel (for multiple subscribers).
pub struct BroadcastSink {
    tx: broadcast::Sender<AgentEvent>,
}

/// Discards all events. Useful for tests and background/silent runs.
pub struct NullSink;

/// Fans out events to multiple sinks.
pub struct CompositeSink {
    sinks: Vec<Box<dyn EventSink>>,
}
```

Implement all four sink types with their constructors and `EventSink` impl.

### 2. Bridge conversion

```rust
impl From<crate::agent::StepEvent> for AgentEvent {
    fn from(event: StepEvent) -> Self {
        // Map each StepEvent variant to the corresponding AgentEvent variant
    }
}
```

This allows gradual migration — existing code emits StepEvent, which can be
converted to AgentEvent for the new sinks.

### 3. Wire into lib.rs

Add `pub mod event;` and re-export key types:
```rust
pub use event::{AgentEvent, EventSink, ChannelSink, BroadcastSink, NullSink, CompositeSink};
```

### 4. Tests

- `channel_sink_delivers_events` — emit through ChannelSink, receive on rx
- `broadcast_sink_delivers_to_multiple` — two subscribers both get events
- `null_sink_does_not_panic` — NullSink can receive any event without error
- `composite_sink_fans_out` — emit once, all inner sinks receive
- `step_event_to_agent_event_conversion` — From<StepEvent> maps correctly
- `agent_event_serialization_round_trip` — serde JSON round-trip

## Acceptance

- `cargo test` green (505+ tests, new tests added)
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- New module `src/event.rs` exists with all types and impls
- `lib.rs` re-exports the public types
- **No changes** to agent.rs, main.rs, http.rs, runner.rs, multi.rs

## Notes for the agent

- Read `src/agent.rs` lines 68-175 (StepEvent and FinishReason enums) to understand the source types you need to bridge FROM.
- The `From<StepEvent>` conversion doesn't need to be perfect for every variant on day one — handle the common ones, use a sensible fallback for the rest.
- AgentEvent uses `String` for finish reason (not the enum directly) to avoid coupling kernel.rs ↔ agent.rs at this stage.
- `CompositeSink` should iterate its inner sinks and emit to each. If one panics, continue to the rest (catch_unwind is optional; just document the contract).
- **DO NOT touch any existing file except `src/lib.rs`** (adding the `pub mod event;` line and re-exports).

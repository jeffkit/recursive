# Events & Observers

Subscribe to the `AgentEvent` stream via an `EventSink` to build UIs, logging systems, replay mechanisms, or tests — without touching the agent loop.

## AgentEvent variants

```rust
#[non_exhaustive]
pub enum AgentEvent {
    AssistantText { text: String, step: usize },
    ToolCall { name: String, id: String, arguments: String, step: usize },
    ToolResult { id: String, name: String, output: String, step: usize },
    Latency { step: usize, llm_ms: u64 },
    Usage { input_tokens: u32, output_tokens: u32, step: usize },
    PartialToken { text: String, step: usize },
    Reasoning { text: String, step: usize },
    Compacted { removed: usize, kept: usize, summary_chars: usize, step: usize },
    TurnFinished { reason: String, steps: usize },
    // ... additional variants
}
```

## Subscribing via ChannelSink

```rust
use recursive::event::{AgentEvent, ChannelSink};
use std::sync::Arc;

let (sink, mut rx) = ChannelSink::new(128);

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .tools(tools)
    .event_sink(Arc::new(sink))
    .build()?;

// Spawn a task to consume events
tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCall { name, arguments, .. } => {
                println!("[tool] {} {}", name, arguments);
            }
            AgentEvent::TurnFinished { reason, steps } => {
                println!("[done] {} steps, reason: {}", steps, reason);
            }
            _ => {}
        }
    }
});

let outcome = runtime.run("your goal").await?;
```

## Subscribing via BroadcastSink

```rust
use recursive::event::BroadcastSink;
use std::sync::Arc;

let (sink, rx) = BroadcastSink::new(128);
// Clone rx for multiple subscribers
let rx2 = sink.subscribe();

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .event_sink(Arc::new(sink))
    .build()?;
```

## Use cases

| Use case | Events to watch |
|---|---|
| Progress indicator | `ToolCall`, `TurnFinished` |
| Streaming output | `PartialToken`, `AssistantText` |
| Cost tracking | `Usage` (accumulate token counts) |
| Latency monitoring | `Latency` |
| Audit logging | All events |
| Replay | All events (serialize to JSONL) |
| Testing | `ToolCall` / `ToolResult` (assert tool calls) |

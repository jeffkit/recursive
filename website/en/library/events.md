# Events & Observers

Subscribe to the `StepEvent` stream to build UIs, logging systems, replay mechanisms, or tests — without touching the agent loop.

## StepEvent variants

```rust
pub enum StepEvent {
    LlmStart {
        step: usize,
        messages: Vec<Message>,
    },
    LlmEnd {
        step: usize,
        message: Message,
    },
    ToolStart {
        step: usize,
        name: String,
        args: Value,
    },
    ToolEnd {
        step: usize,
        name: String,
        result: ToolResult,
    },
    Compacted {
        removed: usize,
        summary_chars: usize,
    },
    Done {
        finish_reason: FinishReason,
        final_message: Option<String>,
    },
}
```

## Subscribing via builder

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .on_event(|event| match event {
        StepEvent::ToolStart { name, args, .. } => {
            println!("[tool] {} {:?}", name, args);
        }
        StepEvent::Done { finish_reason, .. } => {
            println!("[done] {:?}", finish_reason);
        }
        _ => {}
    })
    .build()?;
```

## Subscribing via channel

```rust
use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::unbounded_channel();

let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .event_sender(tx)
    .build()?;

// Spawn a task to consume events
tokio::spawn(async move {
    while let Some(event) = rx.recv().await {
        // handle event
    }
});

let outcome = agent.run("your goal").await?;
```

## Use cases

| Use case | Events to watch |
|---|---|
| Progress indicator | `LlmStart`, `ToolStart`, `Done` |
| Cost tracking | `Done` (check `outcome.token_usage`) |
| Audit logging | All events |
| Replay | All events (serialize to JSONL) |
| Testing | `ToolStart` / `ToolEnd` (assert tool calls) |

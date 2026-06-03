# 事件与观察者

通过 `EventSink` 订阅 `AgentEvent` 流，无需修改 Agent 循环即可构建 UI、日志系统、回放机制或测试。

## AgentEvent 变体

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
    // ... 更多变体
}
```

## 通过 ChannelSink 订阅

```rust
use recursive::event::{AgentEvent, ChannelSink};
use std::sync::Arc;

let (sink, mut rx) = ChannelSink::new(128);

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .tools(tools)
    .event_sink(Arc::new(sink))
    .build()?;

tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::ToolCall { name, arguments, .. } => {
                println!("[工具] {} {}", name, arguments);
            }
            AgentEvent::TurnFinished { reason, steps } => {
                println!("[完成] {} 步，原因：{}", steps, reason);
            }
            _ => {}
        }
    }
});

let outcome = runtime.run("你的目标").await?;
```

## 通过 BroadcastSink 订阅

```rust
use recursive::event::BroadcastSink;
use std::sync::Arc;

let (sink, rx) = BroadcastSink::new(128);
// 可为多个订阅者克隆 rx
let rx2 = sink.subscribe();

let mut runtime = AgentRuntime::builder()
    .llm(llm)
    .event_sink(Arc::new(sink))
    .build()?;
```

## 使用场景

| 场景 | 关注的事件 |
|---|---|
| 进度指示器 | `ToolCall`、`TurnFinished` |
| 流式输出 | `PartialToken`、`AssistantText` |
| 费用追踪 | `Usage`（累计 token 数） |
| 延迟监控 | `Latency` |
| 审计日志 | 所有事件 |
| 回放 | 所有事件（序列化为 JSONL） |
| 测试 | `ToolCall` / `ToolResult`（断言工具调用） |

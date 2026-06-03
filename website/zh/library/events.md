# 事件与观察者

订阅 `StepEvent` 流，无需修改 Agent 循环即可构建 UI、日志系统、回放机制或测试。

## StepEvent 变体

```rust
pub enum StepEvent {
    LlmStart { step: usize, messages: Vec<Message> },
    LlmEnd { step: usize, message: Message },
    ToolStart { step: usize, name: String, args: Value },
    ToolEnd { step: usize, name: String, result: ToolResult },
    Compacted { removed: usize, summary_chars: usize },
    Done { finish_reason: FinishReason, final_message: Option<String> },
}
```

## 通过构建器订阅

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .on_event(|event| match event {
        StepEvent::ToolStart { name, args, .. } => {
            println!("[工具] {} {:?}", name, args);
        }
        StepEvent::Done { finish_reason, .. } => {
            println!("[完成] {:?}", finish_reason);
        }
        _ => {}
    })
    .build()?;
```

## 通过 Channel 订阅

```rust
use tokio::sync::mpsc;

let (tx, mut rx) = mpsc::unbounded_channel();

let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .event_sender(tx)
    .build()?;

tokio::spawn(async move {
    while let Some(event) = rx.recv().await {
        // 处理事件
    }
});

let outcome = agent.run("你的目标").await?;
```

## 使用场景

| 场景 | 关注的事件 |
|---|---|
| 进度指示器 | `LlmStart`、`ToolStart`、`Done` |
| 费用追踪 | `Done`（检查 `outcome.token_usage`） |
| 审计日志 | 所有事件 |
| 回放 | 所有事件（序列化为 JSONL） |
| 测试 | `ToolStart` / `ToolEnd`（断言工具调用） |

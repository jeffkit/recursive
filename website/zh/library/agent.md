# Agent 运行时构建器

`AgentRuntimeBuilder` 提供流式 API 用于构建 `AgentRuntime`。

## 构建器选项

```rust
let mut runtime = AgentRuntime::builder()
    .llm(llm_provider)           // 必填：Arc<dyn LlmProvider>
    .tools(tool_registry)        // 可选：ToolRegistry
    .max_steps(20)               // 可选：步骤预算（默认 32）
    .system_prompt("...")        // 可选：自定义系统提示字符串
    .event_sink(my_sink)         // 可选：Arc<dyn EventSink> 观察者
    .build()?;
```

## 运行 Agent

```rust
// 运行至完成
let outcome = runtime.run("你的目标").await?;

// 访问结果
match outcome.finish_reason {
    FinishReason::NoMoreToolCalls => {
        println!("{}", outcome.final_text.unwrap_or_default());
    }
    FinishReason::BudgetExceeded => {
        eprintln!("Agent 达到步骤预算");
    }
    FinishReason::Stuck { repeated_call, repeats } => {
        eprintln!("Agent 卡住了：{repeated_call} 重复了 {repeats} 次");
    }
    FinishReason::ProviderStop(reason) => {
        println!("Provider 停止：{reason}");
        println!("{}", outcome.final_text.unwrap_or_default());
    }
    _ => {}
}
```

## RuntimeOutcome

```rust
pub struct RuntimeOutcome {
    pub finish_reason: FinishReason,
    pub final_text: Option<String>,
    pub steps: usize,
}
```

## FinishReason

```rust
pub enum FinishReason {
    NoMoreToolCalls,                              // 模型停止调用工具
    BudgetExceeded,                               // 达到 max_steps
    ProviderStop(String),                         // LLM 返回停止信号
    Stuck { repeated_call: String, repeats: usize }, // 同一工具调用循环
    TranscriptLimit { chars: usize, limit: usize },  // 对话记录过大
    PlanPending,                                  // Agent 等待计划审批
    Cancelled,                                    // 运行被外部取消
    PermissionDenialLimit,                        // 权限拒绝次数过多
}
```

> **注意：** 运行时的错误通过 `runtime.run()` 的 `Err(...)` 返回，而不是作为 `FinishReason` 的变体。使用 `?` 或 `match` 处理 `Result`。

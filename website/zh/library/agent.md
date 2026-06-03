# Agent 构建器

`AgentBuilder` 提供流式 API 用于构建 `Agent`。

## 构建器选项

```rust
let agent = Agent::builder()
    .llm(llm_provider)           // 必填：Arc<dyn LlmProvider>
    .tools(tool_registry)        // 可选：ToolRegistry
    .max_steps(20)               // 可选：步骤预算（默认 32）
    .system_prompt("...")        // 可选：自定义系统提示字符串
    .system_prompt_file("path")  // 可选：从文件加载系统提示
    .workspace("./my-project")   // 可选：沙箱根目录
    .temperature(0.2)            // 可选：覆盖温度
    .on_event(|e| { ... })       // 可选：StepEvent 观察者闭包
    .build()?;
```

## 运行 Agent

```rust
// 运行至完成
let outcome = agent.run("你的目标").await?;

// 访问结果
match outcome.finish_reason {
    FinishReason::Done => {
        println!("{}", outcome.final_message.unwrap_or_default());
    }
    FinishReason::BudgetExceeded => {
        eprintln!("Agent 达到步骤预算");
    }
    FinishReason::Error(e) => {
        eprintln!("Agent 错误：{e}");
    }
    _ => {}
}
```

## AgentOutcome

```rust
pub struct AgentOutcome {
    pub finish_reason: FinishReason,
    pub final_message: Option<String>,
    pub steps: usize,
    pub token_usage: Option<TokenUsage>,
    pub cost_usd: Option<f64>,
}
```

## FinishReason

```rust
pub enum FinishReason {
    Done,                    // 模型给出了最终文本答案
    BudgetExceeded,          // 达到 max_steps
    Stuck,                   // 同一工具调用重复 3 次
    NoMoreToolCalls,         // 模型停止调用工具
    TranscriptLimit,         // 对话记录超出压缩限制
    Error(RecursiveError),   // 不可恢复的错误
}
```

# 库 API 概览

Recursive 同时是 CLI 工具和 Rust 库。当 CLI 不是最合适的外壳时，可以将 Agent 循环直接嵌入你自己的程序。

## 添加依赖

```toml
[dependencies]
recursive-agent = "0.6"
tokio = { version = "1", features = ["full"] }
```

## 最简示例

```rust
use std::sync::Arc;
use recursive::{
    Agent, ToolRegistry,
    llm::OpenAiProvider,
    tools::{ApplyPatch, ListDir, ReadFile, RunShell, WriteFile},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let llm = Arc::new(OpenAiProvider::new(
        "https://api.openai.com/v1",
        std::env::var("OPENAI_API_KEY")?,
        "gpt-4o-mini",
    ));

    let tools = ToolRegistry::local()
        .register(Arc::new(ReadFile::new(".")))
        .register(Arc::new(WriteFile::new(".")))
        .register(Arc::new(ApplyPatch::new(".")))
        .register(Arc::new(ListDir::new(".")))
        .register(Arc::new(RunShell::new(".")));

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .max_steps(20)
        .build()?;

    let outcome = agent.run("列出 src 目录的文件并总结").await?;
    println!("{}", outcome.final_message.unwrap_or_default());
    Ok(())
}
```

## 公开 API

库暴露的主要类型：

- `Agent` + `AgentBuilder` — 主入口
- `ToolRegistry` — 注册和分发工具
- `LlmProvider` trait — 实现自定义后端
- `Tool` trait — 实现自定义工具
- `StepEvent` — 订阅事件流
- `FinishReason` — Agent 停止的原因
- `Message`、`Role` — 对话记录原语
- `AgentOutcome` — Agent 的返回值

另见：
- [Agent 构建器](./agent) — 构建器选项
- [自定义 Tool](./tools) — 实现 `Tool` trait
- [自定义 Provider](./providers) — 实现 `LlmProvider`
- [事件与观察者](./events) — `StepEvent` 流
- [多 Agent](./multi-agent) — 池、消息、编排

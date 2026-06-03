# Library API Overview

Recursive is both a CLI tool and a Rust library. Embed the agent loop directly in your own program when the CLI is not the right shell for your use case.

## Adding the dependency

```toml
[dependencies]
recursive-agent = "0.6"
tokio = { version = "1", features = ["full"] }
```

## Minimal example

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

    let outcome = agent.run("list the files in src and summarise them").await?;
    println!("{}", outcome.final_message.unwrap_or_default());
    Ok(())
}
```

## Public API surface

The library exposes:

- `Agent` + `AgentBuilder` — the main entry point
- `ToolRegistry` — registers and dispatches tools
- `LlmProvider` trait — implement your own backend
- `Tool` trait — implement your own tools
- `StepEvent` — subscribe to the event stream
- `FinishReason` — why the agent stopped
- `Message`, `Role` — transcript primitives
- `AgentOutcome` — what the agent returned

See also:
- [Agent Builder](./agent) — builder options
- [Custom Tools](./tools) — implementing the `Tool` trait
- [Custom Providers](./providers) — implementing `LlmProvider`
- [Events & Observers](./events) — the `StepEvent` stream
- [Multi-Agent](./multi-agent) — pools, messaging, orchestration

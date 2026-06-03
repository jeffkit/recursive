# Quick Start

## Installation

### From crates.io

```bash
cargo install recursive-agent
```

> The crate is published as `recursive-agent` because the name `recursive` was taken on crates.io. The installed binary is still called `recursive`.

### From source

```bash
git clone https://github.com/jeffkit/recursive.git
cd recursive
cargo install --path .
```

### Docker

```bash
docker pull ghcr.io/jeffkit/recursive:latest
```

## Prerequisites

You need an LLM API key. Recursive works with any OpenAI-compatible endpoint.

```bash
export RECURSIVE_API_KEY="your-api-key"
export RECURSIVE_API_BASE="https://api.openai.com/v1"   # or any compatible endpoint
export RECURSIVE_MODEL="gpt-4o-mini"
```

## Run your first agent

```bash
recursive run "list the files in the current directory and summarise what this project does"
```

Recursive will:
1. Send your goal to the LLM
2. Execute any tools the model requests (e.g. `list_dir`, `read_file`)
3. Loop until the model produces a final answer or hits the step budget
4. Print the result

## Interactive REPL

```bash
recursive repl
```

One goal per line. Type `:q` to exit.

## Connect to an LLM provider

### OpenAI

```bash
export RECURSIVE_API_KEY="$OPENAI_API_KEY"
export RECURSIVE_API_BASE="https://api.openai.com/v1"
export RECURSIVE_MODEL="gpt-4o"
recursive run "explain src/agent.rs"
```

### Anthropic (Claude)

```bash
export RECURSIVE_API_KEY="$ANTHROPIC_API_KEY"
export RECURSIVE_API_BASE="https://api.anthropic.com"
export RECURSIVE_MODEL="claude-sonnet-4-5"
export RECURSIVE_PROVIDER_TYPE="anthropic"
recursive run "explain src/agent.rs"
```

### GLM / Zhipu

```bash
export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
export RECURSIVE_API_KEY="$GLM_API_KEY"
export RECURSIVE_MODEL="glm-4-flash"
recursive run "create hello.txt and read it back"
```

### DeepSeek

```bash
export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
export RECURSIVE_API_KEY="$DEEPSEEK_API_KEY"
export RECURSIVE_MODEL="deepseek-coder"
recursive run "review the code in src/"
```

### Ollama (local)

```bash
export RECURSIVE_API_BASE="http://localhost:11434/v1"
export RECURSIVE_API_KEY="ollama"
export RECURSIVE_MODEL="qwen2.5-coder"
recursive run "explain the repo layout"
```

## Use as a Rust library

```toml
# Cargo.toml
[dependencies]
recursive-agent = "0.6"
tokio = { version = "1", features = ["full"] }
```

```rust
use std::sync::Arc;
use recursive::{
    runtime::AgentRuntime,
    tools::{ApplyPatch, ListDir, ReadFile, RunShell, ToolRegistry, WriteFile},
    llm::OpenAiProvider,
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

    let mut runtime = AgentRuntime::builder()
        .llm(llm)
        .tools(tools)
        .max_steps(20)
        .build()?;

    let outcome = runtime.run("list the files in src and summarise them").await?;
    println!("{}", outcome.final_text.unwrap_or_default());
    Ok(())
}
```

## Next steps

- [Core Concepts](./concepts) — understand how the loop works
- [CLI Reference](../cli/) — all commands and flags
- [Configuration](./config) — all environment variables

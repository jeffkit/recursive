# 快速开始

## 安装

### 从 crates.io 安装

```bash
cargo install recursive-agent
```

> Crate 发布名为 `recursive-agent`（因为 `recursive` 在 crates.io 已被占用）。安装后的二进制文件仍然叫 `recursive`。

### 从源码构建

```bash
git clone https://github.com/jeffkit/recursive.git
cd recursive
cargo install --path .
```

### Docker

```bash
docker pull ghcr.io/jeffkit/recursive:latest
```

## 前置条件

你需要一个 LLM API Key。Recursive 支持任何 OpenAI 兼容接口。

```bash
export RECURSIVE_API_KEY="your-api-key"
export RECURSIVE_API_BASE="https://api.openai.com/v1"
export RECURSIVE_MODEL="gpt-4o-mini"
```

## 运行你的第一个 Agent

```bash
recursive run "列出当前目录的文件，总结这个项目是做什么的"
```

Recursive 将会：
1. 将目标发送给 LLM
2. 执行模型请求的工具调用（如 `list_dir`、`read_file`）
3. 循环直到模型给出最终答案或达到步骤预算
4. 打印结果

## 交互式 REPL

```bash
recursive repl
```

每行输入一个目标，输入 `:q` 退出。

## 连接 LLM Provider

### OpenAI

```bash
export RECURSIVE_API_KEY="$OPENAI_API_KEY"
export RECURSIVE_API_BASE="https://api.openai.com/v1"
export RECURSIVE_MODEL="gpt-4o"
recursive run "解释 src/agent.rs 的功能"
```

### Anthropic（Claude）

```bash
export RECURSIVE_API_KEY="$ANTHROPIC_API_KEY"
export RECURSIVE_API_BASE="https://api.anthropic.com"
export RECURSIVE_MODEL="claude-sonnet-4-5"
export RECURSIVE_PROVIDER_TYPE="anthropic"
recursive run "解释 src/agent.rs 的功能"
```

### GLM / 智谱

```bash
export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
export RECURSIVE_API_KEY="$GLM_API_KEY"
export RECURSIVE_MODEL="glm-4-flash"
recursive run "创建 hello.txt 并读取内容"
```

### DeepSeek

```bash
export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
export RECURSIVE_API_KEY="$DEEPSEEK_API_KEY"
export RECURSIVE_MODEL="deepseek-coder"
recursive run "审查 src/ 中的代码"
```

### Moonshot（Kimi）

```bash
export RECURSIVE_API_BASE="https://api.moonshot.cn/v1"
export RECURSIVE_API_KEY="$MOONSHOT_API_KEY"
export RECURSIVE_MODEL="moonshot-v1-8k"
recursive run "总结 README.md"
```

### Ollama（本地）

```bash
export RECURSIVE_API_BASE="http://localhost:11434/v1"
export RECURSIVE_API_KEY="ollama"
export RECURSIVE_MODEL="qwen2.5-coder"
recursive run "解释仓库布局"
```

## 作为 Rust 库使用

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

    let outcome = runtime.run("列出 src 目录的文件并总结").await?;
    println!("{}", outcome.final_text.unwrap_or_default());
    Ok(())
}
```

## 下一步

- [核心概念](./concepts) — 了解循环的工作原理
- [CLI 参考](../cli/) — 所有命令和参数
- [配置参考](./config) — 所有环境变量

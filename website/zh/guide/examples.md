# 示例与教程

展示如何用 Recursive 构建真实场景 Agent 的实用示例。

## 示例一：代码审查 Agent

构建一个审查 Pull Request 或代码变更并生成结构化报告的 Agent。

```rust
use std::sync::Arc;
use recursive::{
    Agent, ToolRegistry,
    llm::OpenAiProvider,
    tools::{ListDir, ReadFile, RunShell, SearchFiles},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let llm = Arc::new(OpenAiProvider::new(
        std::env::var("RECURSIVE_API_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        std::env::var("RECURSIVE_API_KEY")?,
        std::env::var("RECURSIVE_MODEL").unwrap_or_else(|_| "gpt-4o".to_string()),
    ));

    let workspace = std::env::var("RECURSIVE_WORKSPACE")
        .unwrap_or_else(|_| ".".to_string());

    let tools = ToolRegistry::local()
        .register(Arc::new(ReadFile::new(&workspace)))
        .register(Arc::new(ListDir::new(&workspace)))
        .register(Arc::new(SearchFiles::new(&workspace)))
        .register(Arc::new(RunShell::new(&workspace)));

    let mut agent = Agent::builder()
        .llm(llm)
        .tools(tools)
        .max_steps(30)
        .system_prompt(
            "你是一名专业代码审查员。分析提供的 diff 或文件，\
             识别 bug、代码风格问题、安全隐患，并给出改进建议。\
             要具体——引用文件名和行号。",
        )
        .build()?;

    let outcome = agent
        .run(
            "运行 `git diff HEAD~1` 获取最新变更。\
             然后详细审查所有修改的文件。\
             生成包含以下章节的结构化报告：\
             摘要、Bug、代码风格问题、安全隐患、改进建议。",
        )
        .await?;

    println!("{}", outcome.final_message.unwrap_or_default());
    Ok(())
}
```

**运行：**
```bash
export RECURSIVE_API_KEY="..."
export RECURSIVE_WORKSPACE="/path/to/your/project"
cargo run --example code-review
```

---

## 示例二：文件整理 Agent

扫描杂乱目录，提出新结构方案，并自动整理文件的 Agent。

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .max_steps(50)
    .system_prompt(
        "你是文件组织专家。分析目录结构，\
         对相关文件分组，整理成清晰的层级结构。\
         始终说明移动的文件和原因。",
    )
    .build()?;

let outcome = agent
    .run(
        "扫描 ~/Downloads 目录。\
         列出所有文件，识别规律（文档、图片、代码等），\
         提出整理方案，然后执行。",
    )
    .await?;
```

---

## 示例三：文档生成 Agent

读取 Rust crate 源文件并生成 Markdown 文档。

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .max_steps(40)
    .system_prompt(
        "你是专注于 Rust 的技术写作者。\
         阅读源文件，理解其用途，编写清晰准确的 Markdown 文档。",
    )
    .build()?;

let outcome = agent
    .run(
        "阅读 src/ 中的所有 .rs 文件。\
         为每个公开的 struct、enum 和函数编写文档注释。\
         然后写一份 MODULE.md，总结模块用途和公开 API。",
    )
    .await?;
```

---

## 运行示例

所有示例都在仓库的 [`examples/`](https://github.com/jeffkit/recursive/tree/main/examples) 目录中：

```bash
git clone https://github.com/jeffkit/recursive.git
cd recursive
export RECURSIVE_API_KEY="your-key"
cargo run --example code-review
```

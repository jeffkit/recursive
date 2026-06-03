# Examples & Recipes

Practical examples showing how to build real-world agents with Recursive.

## Recipe 1: Code Review Agent

Build an agent that reviews a pull request or code change and produces a structured report.

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
            "You are an expert code reviewer. Analyze the provided diff or files, \
             identify bugs, style issues, security concerns, and suggest improvements. \
             Be specific — cite file names and line numbers where relevant.",
        )
        .build()?;

    // Run git diff to get the current changes
    let outcome = agent
        .run(
            "Run `git diff HEAD~1` to see the latest changes. \
             Then review all modified files in detail. \
             Produce a structured report with sections: \
             Summary, Bugs, Style Issues, Security Concerns, and Suggestions.",
        )
        .await?;

    println!("{}", outcome.final_message.unwrap_or_default());
    Ok(())
}
```

**Run it:**
```bash
export RECURSIVE_API_KEY="..."
export RECURSIVE_WORKSPACE="/path/to/your/project"
cargo run --example code-review
```

---

## Recipe 2: File Organizer Agent

An agent that scans a messy directory, proposes a new structure, and reorganizes files.

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .max_steps(50)
    .system_prompt(
        "You are a file organization expert. \
         Analyze the directory structure, group related files, \
         and reorganize them into a clean hierarchy. \
         Always explain what you're moving and why.",
    )
    .build()?;

let outcome = agent
    .run(
        "Scan the ~/Downloads directory. \
         List all files, identify patterns (documents, images, code, etc.), \
         propose a reorganization plan, then execute it.",
    )
    .await?;
```

---

## Recipe 3: Documentation Generator

Generate documentation for a Rust crate by reading source files and producing Markdown.

```rust
let mut agent = Agent::builder()
    .llm(llm)
    .tools(tools)
    .max_steps(40)
    .system_prompt(
        "You are a technical writer specializing in Rust. \
         Read source files, understand their purpose, and write \
         clear, accurate Markdown documentation.",
    )
    .build()?;

let outcome = agent
    .run(
        "Read all .rs files in src/. \
         For each public struct, enum, and function, write a documentation comment. \
         Then write a MODULE.md summarizing the module's purpose and public API.",
    )
    .await?;
```

---

## Recipe 4: Multi-step CI Debug Agent

An agent that reads CI failure logs, diagnoses the root cause, and suggests a fix.

```rust
// First, save the CI log to a file, then:
let outcome = agent
    .run(
        "Read ci-failure.log. \
         Identify the failing test or build step. \
         Read the relevant source files. \
         Diagnose the root cause and propose a minimal fix. \
         If confident, apply the fix using apply_patch.",
    )
    .await?;
```

---

## Running the examples

All these recipes are available in the [`examples/`](https://github.com/jeffkit/recursive/tree/main/examples) directory of the repository:

```bash
git clone https://github.com/jeffkit/recursive.git
cd recursive
export RECURSIVE_API_KEY="your-key"
cargo run --example code-review
```

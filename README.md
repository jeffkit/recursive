# Recursive

A minimal, orthogonal, embeddable coding agent kernel in Rust.

[![Tests](https://img.shields.io/badge/tests-passing-brightgreen)](#testing)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Recursive is a tiny ReAct-style agent loop that wires together:

- an **LLM provider** (OpenAI-compatible HTTP by default; works with OpenAI,
  GLM/Zhipu, DeepSeek, Moonshot, MiniMax, Together, Ollama, vLLM, …)
- a **tool registry** (`read_file`, `write_file`, `apply_patch`, `list_dir`,
  `run_shell`, `count_lines` out of the box; trivially extensible)
- a **transcript** plus a `StepEvent` stream you can observe

The whole kernel is intentionally small enough to read in one sitting.

## At a glance

```rust
use std::sync::Arc;
use recursive::{
    Agent, ToolRegistry,
    llm::OpenAiProvider,
    tools::{ApplyPatch, ListDir, ReadFile, RunShell, WriteFile},
};

# async fn run() -> anyhow::Result<()> {
let llm = Arc::new(OpenAiProvider::new(
    "https://api.openai.com/v1",
    std::env::var("OPENAI_API_KEY")?,
    "gpt-4o-mini",
));

let tools = ToolRegistry::new()
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
# Ok(()) }
```

## Design

The kernel has five concepts, each independently testable:

| Concept | Where | Role |
|---|---|---|
| `Message` | `src/message.rs` | The only data primitive: chat messages with optional tool calls. |
| `LlmProvider` | `src/llm/` | Trait for model backends. Adapters: HTTP (OpenAI-compatible), Mock. |
| `Tool` + `ToolRegistry` | `src/tools/` | Trait for side effects the model can request. Sandboxed to a workspace. |
| `Agent` | `src/agent.rs` | The loop. Receives a goal, alternates model ↔ tools, emits events. |
| `StepEvent` | `src/agent.rs` | Observer channel for UI / logging / replay. |

### Orthogonality

- **New tool?** Implement `Tool`, register it. No agent changes.
- **New model backend?** Implement `LlmProvider`. No tool/agent changes.
- **New UI / observer?** Subscribe to the `StepEvent` channel. No loop changes.
- **New finish reason?** Add a variant to `FinishReason`. Callers can match if they care.

### Safety primitives baked in

- Every fs / shell tool resolves paths through `tools::resolve_within`, which
  rejects anything escaping the configured workspace root.
- `run_shell` enforces a configurable timeout and caps captured output.
- Agent loop respects a step budget (`max_steps`) and emits
  `FinishReason::BudgetExceeded` rather than looping forever.

## CLI

```bash
cargo install --path .   # or once published: cargo install recursive
```

```bash
# one-off goal
recursive run "list files in src and summarise the kernel"

# interactive REPL (one goal per line, :q to exit)
recursive repl

# inspect what tools are registered (no API key needed)
recursive tools
```

### Configuration

Anything OpenAI-compatible works. Override via env vars (or CLI flags):

| Env | Default | Purpose |
|---|---|---|
| `RECURSIVE_API_BASE` | `https://api.openai.com/v1` | Chat-completions endpoint |
| `RECURSIVE_API_KEY` | _(required)_ | Bearer token |
| `RECURSIVE_MODEL` | `gpt-4o-mini` | Model name |
| `RECURSIVE_MAX_STEPS` | `32` | Loop budget |
| `RECURSIVE_TEMPERATURE` | `0.2` | Sampling temperature |
| `RECURSIVE_WORKSPACE` | cwd | Root all fs/shell tools are sandboxed to |
| `RECURSIVE_SYSTEM_PROMPT_FILE` | _(built-in)_ | Path to a system prompt to load |

Example with GLM (Zhipu):

```bash
export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
export RECURSIVE_API_KEY="$GLM_API_KEY"
export RECURSIVE_MODEL="glm-4-flash"
recursive run "create hello.txt and read it back"
```

Example with a local Ollama:

```bash
export RECURSIVE_API_BASE="http://localhost:11434/v1"
export RECURSIVE_API_KEY="ollama"   # ollama ignores it but the field is required
export RECURSIVE_MODEL="qwen2.5-coder"
recursive run "explain the repo layout"
```

## Library API

`recursive` is also a library — embed the loop in your own program if the CLI
isn't the right shell for your use case. See the example above; the public
surface lives in `src/lib.rs`.

## Testing

```bash
cargo test
```

Coverage includes:

- Agent loop: termination, tool dispatch, error recovery, step budget,
  event stream order.
- Tool registry: dispatch, unknown-tool error, path sandboxing.
- Filesystem tools: round-trip, parent-dir creation, sort order, escape
  rejection.
- Shell tool: success / non-zero status / timeout.
- HTTP provider: request shape (with and without tools), response parsing
  (plain text / tool-call), tool-call argument round-trip.
- End-to-end smoke (`tests/smoke.rs`): scripted `MockProvider` driving real
  filesystem tools.

## License

MIT — see [LICENSE](LICENSE).

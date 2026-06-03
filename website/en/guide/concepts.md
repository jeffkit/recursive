# Core Concepts

Recursive has five orthogonal concepts. Each can be replaced or extended without touching the others.

## 1. Message

**Location**: `src/message.rs`

The only data primitive. A `Message` is a chat message with an optional tool-call list. All state flows through `Vec<Message>` — the transcript.

```rust
pub struct Message {
    pub role: Role,          // System | User | Assistant | Tool
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}
```

## 2. LlmProvider

**Location**: `src/llm/`

A trait for model backends. Implement it once, use it everywhere.

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Message>;
}
```

Built-in implementations:
- `OpenAiProvider` — OpenAI-compatible HTTP (works with OpenAI, DeepSeek, Ollama, etc.)
- `AnthropicProvider` — Native Anthropic API
- `MockProvider` — Scripted responses for testing

## 3. Tool + ToolRegistry

**Location**: `src/tools/`

A `Tool` is anything the model can call to produce a side effect (read a file, run a shell command, fetch a URL, etc.).

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;    // JSON Schema
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}
```

`ToolRegistry` maps tool names to implementations and handles dispatch.

**Built-in tools**:

| Tool | Description |
|---|---|
| `read_file` | Read file contents (sandboxed to workspace) |
| `write_file` | Write or create a file |
| `apply_patch` | Apply a V4A-format patch |
| `list_dir` | List directory contents |
| `run_shell` | Execute shell commands (with timeout) |
| `search_files` | Regex search across files |
| `web_fetch` | HTTP GET with HTML extraction (optional) |
| `remember` / `recall` | Persistent key-value memory |

## 4. Agent

**Location**: `src/agent.rs` (the loop itself is `src/run_core.rs`)

The loop. Receives a goal string, alternates model ↔ tools, and runs until a finish condition:

```
goal → [LLM] → tool_calls? → [Tools] → [LLM] → tool_calls? → ... → final_answer
```

**Finish conditions** (`FinishReason`):

| Reason | Meaning |
|---|---|
| `Done` | Model produced a final text answer |
| `BudgetExceeded` | `max_steps` reached |
| `Stuck` | Same tool call repeated 3× |
| `NoMoreToolCalls` | Model stopped calling tools |
| `Error` | Unrecoverable error |

The loop is **intentionally not extensible** — new capabilities belong in tools or providers, not inside the loop.

## 5. StepEvent

**Location**: `src/event.rs`

An observer channel emitted after every step. Subscribe to drive UIs, logging, replay, or testing without touching the loop.

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

**Orthogonality in practice**:

```
┌─────────────────────────────────────────┐
│                  Agent                  │
│  ┌─────────┐         ┌───────────────┐  │
│  │   LLM   │◄────────│ ToolRegistry  │  │
│  │Provider │         │  (Tool × N)   │  │
│  └────┬────┘         └───────────────┘  │
│       │  StepEvent stream               │
└───────┼─────────────────────────────────┘
        ▼
   ┌────────────┐  ┌──────────┐  ┌──────────┐
   │    TUI     │  │ HTTP API │  │  Logger  │
   └────────────┘  └──────────┘  └──────────┘
```

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

## 4. AgentRuntime

**Location**: `src/runtime.rs`

The loop. Receives a goal string, alternates model ↔ tools, and runs until a finish condition:

```
goal → [LLM] → tool_calls? → [Tools] → [LLM] → tool_calls? → ... → final_answer
```

**Finish conditions** (`FinishReason`):

| Reason | Meaning |
|---|---|
| `NoMoreToolCalls` | Model stopped calling tools (normal completion) |
| `ProviderStop(s)` | LLM returned an explicit stop signal |
| `BudgetExceeded` | `max_steps` reached |
| `Stuck { .. }` | Same tool call looping |
| `TranscriptLimit { .. }` | Transcript too large |
| `PlanPending` | Agent paused for plan approval |
| `Cancelled` | Run cancelled externally |
| `PermissionDenialLimit` | Too many permission denials |

Errors during the run are returned as `Err(...)` from `runtime.run()`.

The loop is **intentionally not extensible** — new capabilities belong in tools or providers, not inside the loop.

## 5. AgentEvent

**Location**: `src/event.rs`

An observer channel emitted after every step. Subscribe via an `EventSink` to drive UIs, logging, replay, or testing without touching the loop.

```rust
#[non_exhaustive]
pub enum AgentEvent {
    AssistantText { text: String, step: usize },
    ToolCall { name: String, id: String, arguments: String, step: usize },
    ToolResult { id: String, name: String, output: String, step: usize },
    Usage { input_tokens: u32, output_tokens: u32, step: usize },
    Compacted { removed: usize, kept: usize, summary_chars: usize, step: usize },
    TurnFinished { reason: String, steps: usize },
    // ...
}
```

**Orthogonality in practice**:

```
┌─────────────────────────────────────────┐
│              AgentRuntime               │
│  ┌─────────┐         ┌───────────────┐  │
│  │   LLM   │◄────────│ ToolRegistry  │  │
│  │Provider │         │  (Tool × N)   │  │
│  └────┬────┘         └───────────────┘  │
│       │  AgentEvent stream              │
└───────┼─────────────────────────────────┘
        ▼
   ┌────────────┐  ┌──────────┐  ┌──────────┐
   │    TUI     │  │ HTTP API │  │  Logger  │
   └────────────┘  └──────────┘  └──────────┘
```

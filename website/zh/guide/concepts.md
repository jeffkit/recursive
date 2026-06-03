# 核心概念

Recursive 有五个正交的核心概念，每个概念都可以独立替换，互不影响。

## 1. Message（消息）

**位置**：`src/message.rs`

唯一的数据原语。`Message` 是一条聊天消息，可选包含工具调用列表。所有状态都通过 `Vec<Message>`（对话记录）传递。

```rust
pub struct Message {
    pub role: Role,          // System | User | Assistant | Tool
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}
```

## 2. LlmProvider（LLM 提供商）

**位置**：`src/llm/`

模型后端的 trait。实现一次，到处使用。

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

内置实现：
- `OpenAiProvider` — OpenAI 兼容 HTTP（支持 OpenAI、DeepSeek、Ollama 等）
- `AnthropicProvider` — Anthropic 原生 API
- `MockProvider` — 用于测试的脚本化响应

## 3. Tool + ToolRegistry（工具 + 工具注册表）

**位置**：`src/tools/`

`Tool` 是模型可以调用的任何操作（读取文件、运行 shell 命令、获取 URL 等）。

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;    // JSON Schema
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}
```

`ToolRegistry` 将工具名称映射到实现，负责分发调用。

**内置工具**：

| 工具 | 描述 |
|---|---|
| `read_file` | 读取文件内容（沙箱限制在工作区内） |
| `write_file` | 写入或创建文件 |
| `apply_patch` | 应用 V4A 格式的补丁 |
| `list_dir` | 列出目录内容 |
| `run_shell` | 执行 shell 命令（带超时） |
| `search_files` | 跨文件正则搜索 |
| `web_fetch` | HTTP GET 并提取 HTML 文本（可选） |
| `remember` / `recall` | 持久化键值内存 |

## 4. Agent（Agent 循环）

**位置**：`src/agent.rs`（循环本体在 `src/run_core.rs`）

核心循环。接收目标字符串，交替调用模型和工具，直到满足终止条件：

```
目标 → [LLM] → 工具调用？ → [工具] → [LLM] → 工具调用？ → ... → 最终答案
```

**终止条件**（`FinishReason`）：

| 原因 | 含义 |
|---|---|
| `Done` | 模型给出了最终文本答案 |
| `BudgetExceeded` | 达到 `max_steps` 限制 |
| `Stuck` | 同一工具调用重复 3 次 |
| `NoMoreToolCalls` | 模型停止调用工具 |
| `Error` | 不可恢复的错误 |

循环**故意不可扩展**——新能力应该以工具或 Provider 的形式添加，而不是在循环内部分支。

## 5. StepEvent（步骤事件）

**位置**：`src/event.rs`

每一步执行后发出的观察者通道。订阅它可以驱动 UI、日志、回放或测试，无需修改循环。

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

**正交性示意**：

```
┌─────────────────────────────────────────┐
│                  Agent                  │
│  ┌─────────┐         ┌───────────────┐  │
│  │   LLM   │◄────────│ ToolRegistry  │  │
│  │Provider │         │  (Tool × N)   │  │
│  └────┬────┘         └───────────────┘  │
│       │  StepEvent 流                   │
└───────┼─────────────────────────────────┘
        ▼
   ┌────────────┐  ┌──────────┐  ┌──────────┐
   │    TUI     │  │ HTTP API │  │   日志   │
   └────────────┘  └──────────┘  └──────────┘
```

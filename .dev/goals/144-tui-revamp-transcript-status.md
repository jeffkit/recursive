# Goal 144 — TUI Revamp Step 2: Transcript 块状渲染 + Status Bar + Diff 视图

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (part 2/5)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库
- 消费已有的 `AgentEvent` 变体（`AssistantText` / `PartialToken` / `ToolCall`
  / `ToolResult` / `Usage` / `Latency` / `Compacted`），不要求修改
  `src/event.rs`
- 渲染逻辑放在 `ui/` 子模块，状态放在 `app.rs`，不污染 `backend.rs`

## Why

Goal 143 完成后骨架就位、in-process 通信跑通，但视觉上和 fake-cc 还有
巨大差距：消息仍是单行 `You: ...` / `Agent: ...`；token 计数、模型名、
延迟没有可见入口；`apply_patch` 工具结果是一坨纯文本。

本 goal 把"消息流"和"状态栏"做到 fake-cc 水平的核心 80%（虚拟滚动、
Markdown 渲染、IDE 集成等留到二期）。

## Scope (do exactly this, no more)

### 1. AppState 扩展

在 `app.rs` 给 `AppState` 增加：

```rust
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_input: u64,    // 累计跨 turn
    pub total_output: u64,
    pub last_latency_ms: u64,
}

pub struct TurnState {
    pub running: bool,
    pub started_at: Option<std::time::Instant>,
    pub spinner_verb: &'static str,  // "Thinking" / "Calling tool" / "Reading"
}
```

把现有的 `messages: Vec<StyledMessage>` 升级成块状结构：

```rust
pub enum TranscriptBlock {
    User { text: String },
    Assistant { text: String, streaming: bool },
    ToolCall { id: String, name: String, args_preview: String },
    ToolResult { id: String, name: String, success: bool, output: String, expanded: bool },
    Diff { path: String, hunks: Vec<DiffHunk> },   // 由 apply_patch / write_file 触发
    Compacted { removed: usize, kept: usize },
    System { text: String },
    Error { text: String },
}
```

`ToolResult` 和 `ToolCall` 通过 `id` 配对（`AgentEvent` 已经携带 id）。

### 2. 流式打字（PartialToken）

在 `backend.rs` 把 `AgentEvent::PartialToken { text, .. }` 映射为新事件
`UiEvent::AssistantPartial { text }`。在 `app.rs::apply_event` 中：

- 如果 transcript 末尾不是 `Assistant { streaming: true }`，新建一个并设为
  streaming
- 否则把 text 追加到末尾的 Assistant 块

`AgentEvent::AssistantText { text }` 表示完整一段（非流式 provider 的
fallback）：

- 如果末尾是 streaming Assistant，**用整段文本覆盖**它（确保最终一致），
  并把 `streaming` 置 false
- 否则新建一个 `streaming: false` 的 Assistant 块

### 3. Spinner

在 `ui/spinner.rs` 新建简单 spinner：

- 帧序列 `["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]`（braille）
- 帧速 100ms，由主循环传入 frame index 计算当前字符
- 显示为 transcript 末尾的一个临时行（不写入 `messages` Vec），格式
  `<spinner> <verb> <elapsed>s`，例如 `⠋ Thinking 2.3s`
- 当 `TurnState.running == false` 时不渲染

动词选择：

- `Calling tool` 当最后一个块是 `ToolCall` 且没有对应 `ToolResult`
- `Reading` 当当前 ToolCall 是 `read_file` / `list_dir` / `search_files`
- `Editing` 当是 `apply_patch` / `write_file`
- `Running` 当是 `run_shell`
- `Thinking` 默认

### 4. 块状渲染

在 `ui/transcript.rs`：

每种 `TranscriptBlock` 单独渲染函数，块与块之间留 1 行空白。

#### User 块

```
▎ You
│  hello world
```

边竖线用 `▎`（U+258E），灰色（`Color::DarkGray`）；标题 "You" 用白色加粗。

#### Assistant 块

```
▎ Agent  ⏱ 1.2s
│  这是回复内容
│  支持多行
```

`⏱ Xs` 是 `last_latency_ms`（如有），右对齐到块标题行末尾。

#### ToolCall 块

```
  🔧 read_file  path="src/agent.rs"
```

`args_preview` 取 `args` JSON 中前 1-2 个字段的 key=value 对（不超过
60 字符），其余截断为 `…`。

#### ToolResult 块

```
  ✓ read_file (1.4 KB)
    │ pub fn run() {
    │   ... (124 more lines, press Ctrl+E to expand)
    │ }
```

- 输出超过 6 行时折叠，显示前 3 行 + "… (N more lines, press Ctrl+E
  to expand)"
- `Ctrl+E` 切换 `expanded` 标志
- 失败时图标 `✗` 红色，输出按错误文本渲染

#### Diff 块（特例）

当 `ToolResult` 来源是 `apply_patch` 或 `write_file` 且能解析出 diff
时，用 `Diff` 块替代普通 `ToolResult`。最简方案：

- `apply_patch`：从工具的 input 参数（V4A 格式）里取 `*** Update File: X`
  / `*** Add File: X`，把 `+`/`-` 行按色着色（绿/红）
- `write_file`：标记为 "Created/Updated path/to/file (N bytes)" 加路径
  着色，不展示具体内容

```
  📝 src/agent.rs
    │ - old line
    │ + new line
    │   context line
```

如果 diff 解析失败，回退到普通 `ToolResult`。

### 5. Compacted 通知

`AgentEvent::Compacted { removed, kept, summary_chars }` → 渲染成一个
专门的 `Compacted` 块：

```
  ⊕ Conversation compacted: 12 messages → 1 summary (864 chars)
```

灰色 + ITALIC。

### 6. Status Bar 升级

在 `ui/status.rs`：

底部状态栏从单行升级为信息密集的 2-3 段（用 `│` 分隔）：

```
 local │ deepseek-chat │ ↑1.2k ↓342  $0.0024 │ turn 3 │ ⏱ 2.3s
```

字段：

| 字段 | 来源 | 显示 |
|---|---|---|
| 连接 | 固定 "local" | in-process 模式标记 |
| 模型 | `runtime.kernel().llm().name()` 或环境推断 | 例 `deepseek-chat` |
| token 累计 | `UsageStats.total_input/output` | `↑1.2k ↓342` |
| cost 估算 | input × in_rate + output × out_rate | `$0.0024`，无费率时省略 |
| turn 计数 | `runtime.turn_index()` 或自己累加 | `turn 3` |
| 当前 turn elapsed | `TurnState.started_at.elapsed()` | 仅运行中显示 |

费率表写在 `app.rs` 里硬编码一个 `HashMap<&str, (f64, f64)>` per 1k
tokens，覆盖 4 个常用模型即可（deepseek-chat、gpt-4o、glm-4-plus、
claude-sonnet）。无匹配则不显示 cost。

### 7. 键位新增

只加一个：

- `Ctrl+E`：切换最后一个 `ToolResult` / `Diff` 块的 `expanded` 状态

其余键位保持 Goal 143 的不变。

### 8. 测试

新增/更新：

- `app::transcript_apply_partial_token_appends_to_streaming_assistant`
- `app::transcript_apply_assistant_text_finalizes_streaming`
- `app::tool_call_and_result_pair_by_id`
- `app::compacted_event_creates_compacted_block`
- `app::usage_stats_accumulate_across_turns`
- `app::ctrl_e_toggles_expanded_on_last_tool_result`
- `ui::spinner::spinner_frame_advances_with_index`
- `ui::transcript::tool_result_long_output_truncated_with_hint`
- `ui::transcript::diff_renders_plus_minus_with_colors`（验证 spans 包
  Color::Green/Red）
- `ui::status::status_bar_includes_model_and_tokens`

集成测试：用 MockProvider 跑一个返回 PartialToken 的脚本，验证 transcript
最终块状结构正确。

### 9. 不做的事

- ❌ 多模式输入框（! / # / /） —— 留给 Step 3 (Goal 145)
- ❌ 斜杠命令系统 —— 留给 Step 4 (Goal 146)
- ❌ Plan Mode 协议化 —— 留给 Step 5 (Goal 147)
- ❌ 虚拟滚动（仍是全量渲染 + scroll offset）
- ❌ Markdown 渲染（粗体/斜体/列表先不动）
- ❌ Syntax highlighting

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
3. `cargo fmt --all -- --check` 通过
4. 手工冒烟（验收时）：
   - 启动 TUI，看到 Status Bar 三段格式（不一定有 cost，至少有连接 + 模型）
   - 发一条会触发工具调用的消息，能看到块状 `🔧 / ✓` 渲染（不是单行）
   - 工具结果超过 6 行时显示折叠提示，按 `Ctrl+E` 能展开
   - 如果 provider 支持流式，看到 Assistant 文本逐字浮现
5. 不应回归 Goal 143 的现有键位行为（Enter / ↑↓ / Esc / 字符 /
   Backspace / `q`）

## Notes for the agent

- 先读 Goal 143 完成后的 `crates/recursive-tui/src/` 全部模块
- `AgentRuntime::run` 完成后才返回，期间通过 EventSink 推事件 —— 这意味着
  TUI 主循环必须在 runtime 跑的同时持续 draw（已经是 select! 异步循环，
  天然满足）
- ratatui 0.29 的 spans/lines API：`Line::from(vec![Span::styled(...)])`
- 颜色尽量用 `Color::Indexed` 或命名色（`Color::Green` 等），避免硬编码
  RGB
- spinner 的 frame index 由主循环每次 `draw` 之前累加（每 50ms 一次绘制
  时累加，节流到 100ms 也可）
- 千万不要在 `backend.rs` 里渲染逻辑，渲染只在 `ui/`
- `apply_patch` 的输入 args 是 JSON `{"input": "...V4A..."}`，可以拿
  `tool_call.arguments` 字符串来 parse

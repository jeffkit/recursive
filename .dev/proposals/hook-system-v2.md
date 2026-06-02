# Proposal: Hook System V2 — Full Alignment with Claude Code

> **Status**: Draft
> **Created**: 2026-06-02
> **Baseline**: Current HEAD（`src/hooks/` 已有 HookRegistry + ExternalHookRunner 最小实现）
> **Scope**: 6 个 Gap，3 个 Phase，完整对齐 fake-cc 的 Hook 平台能力

---

## 背景与动机

当前 Recursive 的 Hook 系统（Goal 198 落地）提供了一个最小可用实现：

- `HookRegistry` — Rust trait 生命周期 hooks（SessionStart/PreToolCall/PostToolCall/PreCompact/PostCompact/SessionEnd）
- `ExternalHookRunner` — 扫描 `~/.recursive/hooks/` 可执行文件，JSON stdin/stdout 协议，5s 超时

这是一个**可用的最小安全底座**，但与生产级 Agent（Claude Code / fake-cc）相比存在 6 大 Gap，导致 Recursive 的可扩展性远弱于参照系。

### 参照系

对比对象：fake-cc 源码中的：
- `src/types/hooks.ts` — Hook 类型系统（Sync/Async/输出格式）
- `src/schemas/hooks.ts` — Hook 配置 Schema（command/prompt/http/agent 四种类型）
- `src/utils/hooks/hookEvents.ts` — Hook 执行事件系统（进度/结果流）
- `src/utils/hooks.ts` — Hook 执行引擎

---

## Gap 清单与优先级

| # | Gap | 当前状态 | 影响 | Phase |
|---|-----|---------|------|-------|
| G1 | 事件类型不完整 | 仅 6 种事件 | 缺少 UserPromptSubmit/Stop/PermissionDenied 等 11 种 | P1 |
| G2 | Hook 类型单一 | 只支持 shell 可执行文件 | 无 prompt/http/agent 类型 hook | P2 |
| G3 | 输出解析能力弱 | 只有 continue/skip/error | 无 additionalContext/updatedInput/systemMessage/suppressOutput | P1 |
| G4 | 无异步/后台 Hook | 全部同步阻塞 | 无 async/asyncRewake/once 标志 | P2 |
| G5 | 配置机制原始 | 只能扫描目录 | 无 settings 文件、无 matcher 过滤、超时硬编码 | P1 |
| G6 | TUI 无 Hook 展示 | 无 | Hook 运行时用户无法感知进度 | P3 |

---

## Phase 1 — 基础对齐（G1 / G3 / G5）

### P1-1：扩展 HookEvent 类型（Goal 204）

**目标**：将内部 `HookEvent` 和外部 `ExternalHookRunner::HookEvent` 扩展到与 fake-cc 齐平。

新增内部 Hook 事件（`src/hooks/mod.rs`）：

```rust
#[non_exhaustive]
pub enum HookEvent<'a> {
    // 已有
    SessionStart { goal: &'a str },
    PreToolCall { name: &'a str, args: &'a Value },
    PostToolCall { name: &'a str, args: &'a Value, result: &'a str, duration_ms: u64 },
    PreCompact { transcript_len: usize },
    PostCompact { removed: usize, summary_chars: usize },
    SessionEnd { outcome: &'a AgentOutcome },

    // 新增
    /// 用户消息提交前，AI 尚未处理时触发。
    UserPromptSubmit { content: &'a str },
    /// Agent 正常结束（NoMoreToolCalls）时触发。
    Stop { outcome: &'a AgentOutcome },
    /// 子 Agent 启动前触发。
    SubagentStart { goal: &'a str, depth: usize },
    /// 子 Agent 结束后触发。
    SubagentStop { outcome: &'a AgentOutcome, depth: usize },
    /// 工具调用失败后触发（result 为错误消息）。
    PostToolCallFailure { name: &'a str, args: &'a Value, error: &'a str },
    /// 权限被拒绝时触发。
    PermissionDenied { tool_name: &'a str, reason: &'a str },
    /// Agent 向用户发送通知时触发。
    Notification { message: &'a str },
    /// Setup 阶段（run() 开始前的一次性初始化）。
    Setup,
}
```

外部 Hook 协议事件（`src/hooks/external.rs`）同步扩展：
```rust
pub enum HookEvent {
    PreToolCall, PostToolCall, PostToolCallFailure,
    PermissionRequest, PermissionDenied,
    SessionStart, SessionEnd,
    UserPromptSubmit,
    Stop, SubagentStart, SubagentStop,
    Notification, Setup,
}
```

**单元测试**：每个新事件都能被 `HookRegistry::dispatch` 正确路由。

---

### P1-2：扩展 Hook 输出格式（Goal 205）

**目标**：外部 Hook 可返回富输出字段，对齐 fake-cc 的 `SyncHookJSONOutput`。

扩展 `HookOutput`（`src/hooks/external.rs`）：

```rust
pub struct HookOutput {
    /// continue / skip / error（保持向后兼容）
    pub action: JsonAction,
    /// 错误/跳过原因（原有）
    pub message: Option<String>,

    /// 新增：注入给 LLM 的额外上下文（追加到 system prompt）
    pub additional_context: Option<String>,
    /// 新增：修改工具调用的入参（PreToolCall 生效）
    pub updated_input: Option<serde_json::Value>,
    /// 新增：向用户展示的警告消息
    pub system_message: Option<String>,
    /// 新增：不将 hook stdout 写入 transcript
    pub suppress_output: Option<bool>,
    /// 新增：权限决策（PreToolCall / PermissionRequest 生效）
    /// "allow" / "deny" / "passthrough"
    pub permission_decision: Option<String>,
    /// 新增：权限决策原因（日志/审计）
    pub permission_decision_reason: Option<String>,
}
```

返回新类型 `HookResult`（取代简单的 `HookAction`）：

```rust
pub struct HookResult {
    pub action: HookAction,
    pub additional_context: Option<String>,
    pub updated_input: Option<serde_json::Value>,
    pub system_message: Option<String>,
    pub suppress_output: bool,
    pub permission_decision: Option<PermissionDecision>,
}
```

调用方（`ToolRegistry::invoke_with_audit`）消费 `HookResult`，将 `additional_context` 追加到下一轮 LLM 消息，`updated_input` 替换工具入参，`system_message` 写入 `StepEvent`。

---

### P1-3：Settings 文件 + Matcher 过滤（Goal 206）

**目标**：Hook 配置从目录扫描迁移为 settings 文件，支持 matcher 过滤和每个 hook 单独超时。

配置格式（`~/.recursive/hooks.json` 或 `<workspace>/.recursive/hooks.json`）：

```json
{
  "PreToolCall": [
    {
      "matcher": "run_shell(git *)",
      "hooks": [
        {
          "type": "command",
          "command": "~/.recursive/hooks/git-guard.sh",
          "timeout": 10,
          "statusMessage": "Checking git command safety..."
        }
      ]
    }
  ],
  "UserPromptSubmit": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "~/.recursive/hooks/prompt-logger.sh",
          "async": true
        }
      ]
    }
  ]
}
```

Schema（Rust serde）：
```rust
pub struct HookConfig {
    // HookEvent -> Vec<HookMatcher>
    pub events: HashMap<String, Vec<HookMatcher>>,
}

pub struct HookMatcher {
    pub matcher: Option<String>,   // glob/rule pattern，None = match all
    pub hooks: Vec<HookCommand>,
}

pub struct HookCommand {
    pub r#type: HookCommandType,  // command | http | prompt | agent
    pub command: Option<String>,  // for type=command
    pub url: Option<String>,      // for type=http
    pub prompt: Option<String>,   // for type=prompt/agent
    pub timeout: Option<u64>,     // seconds，default 5
    pub status_message: Option<String>,
    pub once: Option<bool>,
    pub r#async: Option<bool>,
}
```

`ExternalHookRunner::discover()` 改为 `ExternalHookRunner::from_config()`，同时保留目录扫描作为向后兼容的 fallback。

Matcher 语法（简化版）：
- `None` — 匹配所有
- `"run_shell"` — 工具名精确匹配
- `"run_shell(git *)"` — 工具名 + 参数前缀匹配（`command` 参数以 `git ` 开头）
- `"write_file(src/*)"` — 工具名 + 路径前缀匹配（`path` 参数以 `src/` 开头）

---

## Phase 2 — Hook 类型扩展（G2 / G4）

### P2-1：HTTP Hook 类型（Goal 207）

**目标**：支持 `type: "http"` 将 hook 事件 POST 到 webhook URL。

```rust
// HookCommandType::Http
pub struct HttpHookConfig {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    pub timeout: Option<u64>,
    pub status_message: Option<String>,
}
```

执行：`reqwest::Client::post(url).json(input).send().await`，响应体按同样的 `HookOutput` JSON 解析。

### P2-2：Prompt Hook 类型（Goal 208）

**目标**：支持 `type: "prompt"` 用 LLM 评估 hook 输入，无需写 shell 脚本。

```rust
pub struct PromptHookConfig {
    pub prompt: String,    // $ARGUMENTS 占位符会被替换为 hook JSON 输入
    pub model: Option<String>,
    pub timeout: Option<u64>,
    pub status_message: Option<String>,
}
```

执行：将 `prompt` 中的 `$ARGUMENTS` 替换为序列化的 `HookInput` JSON，调用配置的 LLM，解析返回的 `HookOutput`。

### P2-3：异步 Hook 支持（Goal 209）

**目标**：支持 `async: true` 和 `asyncRewake: true` 标志，让 hook 在后台运行不阻塞 Agent。

- `async: true`：spawn hook 进程到后台，立即返回 `HookResult { action: Continue, ... }`
- `asyncRewake: true`：同上，但监听进程退出码，若为 2 则通过 `CancellationToken` 中断当前 Agent 轮次（类似 signal），注入一条 system 消息

`once: true`：执行后从配置中移除，不再触发（用于一次性 setup 类 hook）。

---

## Phase 3 — TUI 集成（G6）

### P3-1：Hook 进度展示（Goal 210）

**目标**：Hook 运行时在 TUI 状态栏展示进度。

新增 `StepEvent` 变体：
```rust
StepEvent::HookStarted { event: String, hook_name: String, status_message: Option<String> },
StepEvent::HookProgress { event: String, hook_name: String, stdout: String },
StepEvent::HookFinished { event: String, hook_name: String, outcome: String, duration_ms: u64 },
```

TUI 处理：
- `HookStarted` → 在 spinner 行追加 `[hook: {status_message}]` 或 `[hook: {hook_name}]`
- `HookProgress` → 更新 spinner 行（last stdout line）
- `HookFinished` → 清除 spinner hook 行（或展示 brief summary）

---

## Goal 分解与依赖图

```
G1: 事件类型扩展 (Goal 204)
G3: 输出格式扩展 (Goal 205) ← 依赖 G1
G5: Settings + Matcher (Goal 206) ← 依赖 G1, G3
G2a: HTTP Hook (Goal 207) ← 依赖 G5
G2b: Prompt Hook (Goal 208) ← 依赖 G5
G4: Async Hook (Goal 209) ← 依赖 G5
G6: TUI 展示 (Goal 210) ← 依赖 G4
```

**推进顺序**（并行友好）：
1. Goal 204（事件扩展）→ 2. Goal 205（输出格式）→ 3. Goal 206（配置系统）
4. Goals 207/208/209 可并行
5. Goal 210 最后

---

## Done 标准（整体）

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- 所有 fake-cc hook 事件类型在 Recursive 中均有对应实现
- `hooks.json` 配置文件可被加载、验证、执行
- HTTP hook 可将 JSON POST 到 localhost 并接收响应
- Prompt hook 可调用配置的 LLM 得到决策
- Async hook 不阻塞 Agent 主循环
- TUI 中 hook 运行时有可见状态反馈

# Goal 204 — Hook System V2 P1-1: 扩展 HookEvent 类型

**Roadmap**: Hook System V2 — Phase 1 基础对齐
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 198（ExternalHookRunner）、Goal 199（headless 权限路径）

**Design principle check**:
- 修改 `src/hooks/mod.rs` — 新增 HookEvent 变体（#[non_exhaustive]，向后兼容）
- 修改 `src/hooks/external.rs` — 同步扩展外部协议枚举
- ❌ 不在 `agent.rs::Agent::run` 中添加新分支

## Why

当前 Recursive 的内部 HookEvent 只有 6 种，外部协议只有 3 种。
fake-cc 支持 18+ 种事件，导致 Recursive 无法拦截：
- 用户消息提交前（UserPromptSubmit）
- Agent 正常结束时（Stop）
- 工具调用失败时（PostToolCallFailure）
- 权限被拒绝时（PermissionDenied）
- 子 Agent 生命周期（SubagentStart/SubagentStop）
- Agent 通知事件（Notification）

## Scope

### 1. 扩展 `src/hooks/mod.rs` — 内部 HookEvent

在现有变体后追加（保持 `#[non_exhaustive]`）：

```rust
/// 用户提交消息后、LLM 处理前触发。
UserPromptSubmit {
    /// 用户输入的内容。
    content: &'a str,
},
/// Agent 正常结束（NoMoreToolCalls）时触发。
Stop {
    /// 最终 outcome。
    outcome: &'a AgentOutcome,
},
/// 子 Agent 启动前触发。
SubagentStart {
    /// 子 Agent 的目标文本。
    goal: &'a str,
    /// 当前嵌套深度（0 = 顶层）。
    depth: usize,
},
/// 子 Agent 结束后触发。
SubagentStop {
    /// 子 Agent 的 outcome。
    outcome: &'a AgentOutcome,
    /// 嵌套深度。
    depth: usize,
},
/// 工具调用返回错误时触发（在 PostToolCall 之前）。
PostToolCallFailure {
    /// 工具名。
    name: &'a str,
    /// 传入参数。
    args: &'a Value,
    /// 错误消息。
    error: &'a str,
},
/// 权限被拒绝时触发。
PermissionDenied {
    /// 被拒绝的工具名。
    tool_name: &'a str,
    /// 拒绝原因描述。
    reason: &'a str,
},
/// Agent 向用户发送通知时触发。
Notification {
    /// 通知内容。
    message: &'a str,
},
/// Session 开始前的一次性 setup 阶段。
Setup,
```

同时在 `HookRegistry::dispatch` 中：
- 新变体默认对 `Skip`/`Error` 与 `PostToolCall` 相同处理（转为 Continue）
- 仅 `PreToolCall` 有短路语义（已有逻辑）

### 2. 扩展 `src/hooks/external.rs` — 外部协议 HookEvent

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookEvent {
    // 已有
    PreToolCall,
    PostToolCall,
    PermissionRequest,
    // 新增
    PostToolCallFailure,
    PermissionDenied,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Stop,
    SubagentStart,
    SubagentStop,
    Notification,
    Setup,
}
```

对应 `HookInput` 新增可选字段（None 时 JSON 序列化忽略）：
```rust
pub struct HookInput {
    pub event: HookEvent,
    pub tool_name: Option<String>,   // 改为 Option，非工具事件时为 None
    pub args: Option<serde_json::Value>,
    pub mode: String,
    // 新增
    pub content: Option<String>,     // UserPromptSubmit 时的用户输入
    pub message: Option<String>,     // Notification 时的消息
    pub depth: Option<usize>,        // SubagentStart/SubagentStop 时的深度
    pub reason: Option<String>,      // PermissionDenied 时的原因
    pub error: Option<String>,       // PostToolCallFailure 时的错误
}
```

### 3. 在 `Agent::run` 中触发新事件

- `UserPromptSubmit`：在每轮用户消息处理前 `hooks.dispatch(UserPromptSubmit { content })`
- `Stop`：在 `FinishReason::NoMoreToolCalls` 返回前触发
- `PostToolCallFailure`：在工具调用返回 `Err` 时，在已有 `PostToolCall` 逻辑附近触发
- `PermissionDenied`：在 `ToolRegistry::invoke` 返回 `PermissionDenied` 错误时触发
- `Notification`：由 `StepEvent::Notification` 派生（如有）
- `Setup`：在 `SessionStart` 之前触发一次

SubagentStart/SubagentStop 由 sub_agent 工具触发（`src/tools/sub_agent.rs`）。

## Tests to add

1. `new_events_compile_and_dispatch` — 各新变体能被 `HookRegistry::dispatch` 调用不 panic
2. `user_prompt_submit_dispatched_before_llm` — 在 mock agent 中验证事件顺序
3. `post_tool_call_failure_dispatched_on_error` — 工具失败时事件被触发
4. `external_hook_event_serialization` — 新 `HookEvent` 枚举序列化为正确的 camelCase
5. `hook_input_optional_fields` — 非工具事件中 `tool_name`/`args` 为 None

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- 所有已有 Hook 测试继续通过（向后兼容）
- 新事件能被内部和外部 hook 正确接收

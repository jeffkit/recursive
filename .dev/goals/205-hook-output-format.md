# Goal 205 — Hook System V2 P1-2: 扩展 Hook 输出格式

**Roadmap**: Hook System V2 — Phase 1 基础对齐
**提案**: `.dev/proposals/hook-system-v2.md`
**依赖**: Goal 204（扩展 HookEvent）

**Design principle check**:
- 修改 `src/hooks/external.rs` — 扩展 `HookOutput`，新增 `HookResult` 类型
- 修改 `src/tools/mod.rs` — `ToolRegistry::invoke_with_audit` 消费 `HookResult`
- ❌ 不改变 `HookRegistry` 的 Rust trait 接口（那是内部 API）

## Why

当前外部 Hook 只能返回 `{"action": "continue/skip/error"}`，3 个动作。
fake-cc 的 hook 可以返回：
- `additionalContext` — 向 LLM 注入额外上下文
- `updatedInput` — 修改工具入参（PreToolCall）
- `systemMessage` — 向用户展示警告
- `suppressOutput` — 隐藏 hook 输出
- `permissionDecision` — 直接下发权限决策（allow/deny）

这些能力对于构建安全策略 hook 至关重要。

## Scope

### 1. 扩展 `HookOutput`（`src/hooks/external.rs`）

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput {
    // 已有
    action: JsonAction,
    #[serde(default)]
    message: Option<String>,

    // 新增
    /// 追加到下一轮 system prompt 的额外上下文。
    #[serde(default)]
    additional_context: Option<String>,
    /// 覆盖工具入参（仅 PreToolCall 生效）。
    #[serde(default)]
    updated_input: Option<serde_json::Value>,
    /// 展示给用户的警告消息（写入 StepEvent::SystemMessage）。
    #[serde(default)]
    system_message: Option<String>,
    /// 为 true 时不将 hook stdout 写入 transcript。
    #[serde(default)]
    suppress_output: bool,
    /// 权限决策："allow" / "deny" / "passthrough"
    #[serde(default)]
    permission_decision: Option<String>,
    /// 权限决策原因（日志/审计）。
    #[serde(default)]
    permission_decision_reason: Option<String>,
}
```

### 2. 新增 `HookResult` 类型（公开）

```rust
#[derive(Debug, Clone)]
pub struct HookResult {
    /// 主动作（向后兼容 HookAction）。
    pub action: HookAction,
    /// 追加给 LLM 的上下文。
    pub additional_context: Option<String>,
    /// 覆盖的工具入参（None = 不覆盖）。
    pub updated_input: Option<serde_json::Value>,
    /// 向用户展示的警告。
    pub system_message: Option<String>,
    /// 不将 hook stdout 写入 transcript。
    pub suppress_output: bool,
    /// 权限决策。
    pub permission_decision: Option<PermissionDecision>,
    /// 权限决策原因。
    pub permission_decision_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    Passthrough,
}
```

### 3. `ExternalHookRunner::dispatch` 返回 `HookResult`

```rust
pub async fn dispatch(&self, input: &HookInput) -> HookResult {
    for hook in &self.hooks {
        let result = self.run_hook(hook, input).await;
        match result {
            Ok(r) if !matches!(r.action, HookAction::Continue) => return r,
            _ => continue,
        }
    }
    HookResult {
        action: HookAction::Continue,
        ..Default::default()
    }
}
```

### 4. 调用方消费 `HookResult`

在 `src/tools/mod.rs`（`ToolRegistry::invoke_with_audit`）：
- `updated_input`：在调用工具前替换 `arguments`
- `system_message`：发送 `StepEvent::SystemMessage { text }` 到事件通道
- `additional_context`：追加到下一条 LLM User 消息（通过已有 injection 机制）
- `permission_decision`：影响权限检查结果（Allow 跳过权限确认，Deny 直接拒绝）

### 5. `StepEvent` 新增变体（`src/event.rs` 或 `src/agent.rs`）

```rust
StepEvent::HookSystemMessage { text: String },
```

TUI 处理：展示为黄色 system 消息块。

## Tests to add

1. `hook_output_parses_additional_context` — JSON 中含 additionalContext 时正确解析
2. `hook_output_parses_updated_input` — updatedInput 正确覆盖工具入参
3. `hook_output_parses_permission_decision` — allow/deny/passthrough 正确映射
4. `hook_result_default_is_continue_with_no_extras` — Default impl 验证
5. `dispatch_returns_full_hook_result` — 包含富字段的 hook 脚本被正确执行和解析
6. `updated_input_replaces_tool_args` — 集成测试：hook 修改入参后工具收到新值

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- 外部 hook 脚本可返回 `additionalContext` 并被注入到 LLM
- 外部 hook 脚本可返回 `updatedInput` 并被工具收到
- 外部 hook 脚本可返回 `systemMessage` 并在 TUI/stderr 展示

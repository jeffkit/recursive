# Goal 161 — TUI: Permission Request Modal (runtime hook + TUI 弹框)

**Roadmap**: TUI 体验提升系列 (part 4/4)

**Design principle check**:
- Runtime 侧：在 `src/runtime.rs` 加 `PermissionHook` trait + mpsc 通道
- TUI 侧：新增 `Modal::Permission` 变体 + dispatch
- 最小侵入：不改 `agent.rs` / `kernel.rs`；hook 接入点在 `ToolRegistry::invoke_with_audit`（Goal 153 添加）之前

## Why

fake-cc gap 文档 §7/§8 把 Permission request modal 列为**下一期重点**（🔴）。
当前 Recursive 的工具权限系统（Goal 133/140）在启动时静态配置，
用户没有运行时按工具选择 allow/deny 的机会。
对日常使用来说，这会迫使用户要么给所有工具完全权限，要么在配置文件里写复杂 allow/deny 规则。

## Scope

### 1. `PermissionHook` trait（src/tools/mod.rs）

```rust
#[async_trait::async_trait]
pub trait PermissionHook: Send + Sync {
    /// Called before every tool dispatch. Return `true` to allow, `false` to deny.
    async fn ask_permission(
        &self,
        tool_name: &str,
        args_preview: &str,
    ) -> bool;
}
```

`ToolRegistry` 增加可选字段 `permission_hook: Option<Arc<dyn PermissionHook>>`，
在 `invoke_with_audit` 开头调用（Goal 153 添加的 `invoke_with_audit`）：
- hook 返回 false → 跳过执行，返回 `Err(Error::PermissionDenied { tool_name })`
- hook 为 None → 无变化（现有行为）

### 2. TUI 侧 PermissionHook 实现（backend.rs）

```rust
struct TuiPermissionHook {
    ask_tx: mpsc::UnboundedSender<PermissionRequest>,
}

struct PermissionRequest {
    tool_name: String,
    args_preview: String,
    response_tx: oneshot::Sender<bool>,
}
```

`TuiPermissionHook::ask_permission` 发送 `PermissionRequest` 到 UI 线程，
然后 `.await` `response_tx` 的结果（阻塞 worker 直到用户响应）。

### 3. 新 Modal 变体

```rust
Modal::Permission {
    tool_name: String,
    args_preview: String,
    response_tx: oneshot::Sender<bool>,
}
```

渲染（仿 `Modal::Confirm`）：

```
╭─ Permission Request ───────────────────────────╮
│                                                  │
│ Tool: run_shell                                  │
│ Args: cargo test --release                       │
│                                                  │
│ Allow this tool call?                            │
│                                                  │
╰──────────────────────────────────────────────────╯
  [y/Enter] Allow   [n/Esc] Deny   [a] Allow All
```

`[a] Allow All` 把当前 session 内对该 tool 的后续请求全部自动允许
（存在 `AppState.auto_allowed_tools: HashSet<String>`）。

### 4. 键位

| 键 | 动作 |
|---|---|
| y / Enter | 发送 true，弹掉 modal |
| n / Esc | 发送 false，弹掉 modal |
| a | 把该 tool 加入 auto_allowed，发送 true，弹掉 modal |

### 5. 配置开关

`/permissions on|off` 命令 → 开关 `AppState.permission_hook_enabled`。
默认 off（向后兼容现有行为）。

### 6. Tests

Runtime 侧（unit）：
- `permission_hook_deny_returns_permission_denied_error`
- `permission_hook_allow_proceeds_normally`
- `permission_hook_none_allows_all`

TUI 侧（unit）：
- `permission_modal_renders_tool_and_args`
- `permission_modal_y_sends_true_and_pops`
- `permission_modal_n_sends_false_and_pops`
- `permission_modal_a_adds_to_auto_allowed`
- `auto_allowed_tool_skips_modal`

### 7. 不做的事

- ❌ 持久化权限策略到 config（依赖 session 持久化）
- ❌ 细粒度 args 过滤（允许某工具的某些参数）
- ❌ 与 Goal 133/140 的静态 allow/deny 合并（保持独立层）

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy -- -D warnings` 无警告
3. 手工冒烟：`/permissions on`，发一条需要 run_shell 的消息，弹出 Permission modal；
   按 y 继续，按 n 拒绝并显示 "Permission denied: run_shell"；
   按 a 后续同 tool 不再弹出

**依赖**: Goal 153（`invoke_with_audit` 接入点）

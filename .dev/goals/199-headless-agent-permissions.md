# Goal 199 — P3-3: 无头 Agent 权限路径

**Roadmap**: Permission System V2 — Phase 3 运行时控制

**依赖**: Goal 190（PermissionMode）、Goal 198（ExternalHookRunner）

**Design principle check**:
- 修改 `src/agent.rs` 的 `AgentConfig`（仅添加字段，不动主循环逻辑）
- 修改 `src/tools/mod.rs` 的 `ToolRegistry::invoke()`，添加 headless 分支
- ❌ 不在 `agent.rs::Agent::run` 中添加新分支

## Why

在 CI、云部署、自动化流水线中运行 Recursive 时，没有终端用户可以回答
"是否允许此工具调用"。当前遇到 interactive 列表中的工具会卡住或 panic。
无头模式需要一条明确的决策路径：先走外部 hook，hook 无决策则自动 deny。

## Scope

### 1. `src/agent.rs` — `AgentConfig` 新增字段

```rust
pub struct AgentConfig {
    // ... 现有字段 ...
    /// 无头模式：interactive 工具不等待用户输入，走 hook 或自动 deny。
    #[serde(default)]
    pub headless: bool,
}
```

CLI flag（`main.rs`）：`--headless` / `-H`，对应 `AgentConfig::headless = true`。

环境变量支持：`RECURSIVE_HEADLESS=1` 自动启用（`main.rs` 启动时检测）。

### 2. `src/tools/mod.rs` — `ToolRegistry::invoke()` headless 分支

在 `Passthrough` / interactive 判断处插入：

```rust
// interactive 工具处理
if perms.any_interactive(tool_name) {
    if config.headless {
        // 先走外部 hooks
        let hook_input = HookInput {
            event: HookEvent::PermissionRequest,
            tool_name: tool_name.to_string(),
            args: arguments.clone(),
            mode: format!("{:?}", perms.mode),
        };
        let hook_action = hook_runner.dispatch(&hook_input).await;
        match hook_action {
            HookAction::Continue => { /* hook 放行，继续执行工具 */ }
            HookAction::Skip | HookAction::Error => {
                return Err(Error::PermissionDenied {
                    reason: DecisionReason::Hook { name: "PermissionRequest".into() },
                    message: format!(
                        "headless mode: tool `{tool_name}` requires interaction, auto-denied"
                    ),
                });
            }
        }
    } else {
        // 交互模式：现有逻辑（询问用户）
        // ...
    }
}
```

### 3. `ToolRegistry` 持有 `ExternalHookRunner`

```rust
pub struct ToolRegistry {
    // ... 现有字段 ...
    pub hook_runner: ExternalHookRunner,
    pub headless: bool,
}
```

构造时传入：

```rust
ToolRegistry::new(tools, permissions, hook_runner, headless)
```

### 4. `PermissionMode::DontAsk` 与 headless 的关系

- `DontAsk`：直接 deny，不走 hook（静默拒绝）
- `headless=true`：先走 hook，hook 无决策才 deny（有 hook 机会介入）

两者互补；`DontAsk` 适合完全自动化不需 hook 的场景。

### 5. 单元测试

- `headless_interactive_tool_denied_without_hooks`:
  headless=true, hook_runner 无 hooks, interactive 工具 → PermissionDenied
- `headless_interactive_tool_allowed_by_hook`:
  headless=true, mock hook 返回 Continue, interactive 工具 → 执行成功
- `non_headless_interactive_not_auto_denied`:
  headless=false → 不走自动 deny 路径（现有交互逻辑）
- `headless_env_var_sets_config`:
  `RECURSIVE_HEADLESS=1` → `AgentConfig::headless == true`

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `--headless` flag 启动后，interactive 工具在无 hook 情况下返回 PermissionDenied
- `RECURSIVE_HEADLESS=1` 等效于 `--headless`
- 非 headless 模式行为完全不变

## Notes for the agent

- `AgentConfig::headless` 只是一个 bool 字段，不是 `Agent::run` 的新分支；
  逻辑在 `ToolRegistry::invoke()` 中处理，符合不侵入主循环的原则。
- `ExternalHookRunner` 在无 hook 目录时构造为空（`discover` 返回空列表）；
  无需特殊处理，`dispatch` 对空列表直接返回 Continue。
- 测试中可用 `ExternalHookRunner { hooks: vec![] }` 构造无 hook 情况；
  有 hook 测试用临时目录写一个 shell 脚本并 chmod +x。

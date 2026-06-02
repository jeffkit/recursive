# Goal 193 — P1-1b: PermissionMode 语义集成到 check_static()

**Roadmap**: Permission System V2 — Phase 1 基础架构

**依赖**: Goal 190（PermissionMode 枚举）、Goal 191（LayeredPermissionsConfig）、Goal 192（DecisionReason）

**Design principle check**:
- 修改 `src/permissions.rs` 中的 `check_static()`
- ❌ 不修改 `agent.rs` 主循环

## Why

Goal 190 定义了 `PermissionMode` 枚举，但 `check_static()` 尚未根据 mode 改变
决策逻辑。本 Goal 将 mode 语义真正集成进权限检查流程，使 plan-mode 能拦截写操作、
bypassPermissions 能跳过规则检查、dontAsk 能将交互工具转为 deny。

## Scope

### `src/permissions.rs` — `check_static()` 重写

按优先级从高到低依次检查：

```rust
pub fn check_static(&self, tool_name: &str, is_readonly: bool) -> Permission {
    // 1. Plan mode — 拦截写工具（exit_plan_mode 豁免）
    if let PermissionMode::Plan { bypass_available, .. } = &self.mode {
        if !is_readonly && tool_name != "exit_plan_mode" {
            if !bypass_available {
                return Permission::Denied(
                    DecisionReason::Mode(self.mode.clone()),
                    format!("write tools are blocked in plan mode"),
                );
            }
            // bypass_available: 写操作继续往下走规则检查
        }
    }

    // 2. BypassPermissions — 跳过 allow/deny（安全路径保护在 Phase 2 加入）
    if matches!(self.mode, PermissionMode::BypassPermissions) {
        return Permission::Allowed(DecisionReason::Mode(self.mode.clone()));
    }

    // 3. DontAsk — 交互列表工具转为 deny
    if matches!(self.mode, PermissionMode::DontAsk) {
        if self.any_interactive(tool_name) {
            return Permission::Denied(
                DecisionReason::Mode(self.mode.clone()),
                format!("tool `{tool_name}` requires interaction but mode is dontAsk"),
            );
        }
    }

    // 4. AcceptEdits — 工作区内写操作自动放行
    if matches!(self.mode, PermissionMode::AcceptEdits) && !is_readonly {
        return Permission::Allowed(DecisionReason::Mode(self.mode.clone()));
    }

    // 5. 原有 deny/allow 规则（多层合并，Goal 191 提供的 all_deny / all_allow）
    for pattern in self.all_deny() {
        if matches_pattern(pattern, tool_name) {
            return Permission::Denied(
                DecisionReason::Rule { source: RuleSource::User, pattern: pattern.to_string() },
                format!("tool `{tool_name}` matches deny pattern `{pattern}`"),
            );
        }
    }
    for pattern in self.all_allow() {
        if matches_pattern(pattern, tool_name) {
            return Permission::Allowed(
                DecisionReason::Rule { source: RuleSource::User, pattern: pattern.to_string() },
            );
        }
    }

    // 6. 默认：interactive 列表或未知工具走询问
    Permission::Passthrough
}
```

**`is_readonly` 来源**：`Tool` trait 新增 `fn is_readonly(&self) -> bool { false }`，
各读类工具（`read_file`、`glob`、`search_code` 等）实现返回 `true`。
`ToolRegistry::invoke()` 调用前获取该值并传入。

### 单元测试

- `plan_mode_blocks_write`: mode=Plan, is_readonly=false → Denied(Mode)
- `plan_mode_allows_exit`: mode=Plan, tool="exit_plan_mode" → 不走 Plan 拦截
- `plan_mode_bypass_write_continues`: mode=Plan{bypass_available:true}, write tool → 继续往下
- `bypass_skips_deny_rules`: mode=BypassPermissions, tool 在 deny 列表 → Allowed
- `dontask_converts_interactive`: mode=DontAsk, tool 在 interactive 列表 → Denied
- `accept_edits_allows_write`: mode=AcceptEdits, is_readonly=false → Allowed
- `deny_rule_takes_effect`: mode=Default, tool 在 deny 列表 → Denied(Rule)
- `allow_rule_takes_effect`: mode=Default, tool 在 allow 列表 → Allowed(Rule)

## Acceptance

- `cargo test --workspace` 绿色（含上述 8 个测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- 所有 mode 语义按规格表现

## Notes for the agent

- `is_readonly` 标记需要在 `Tool` trait 添加默认方法，read_file/glob/list_files
  等工具覆盖返回 `true`；shell/write_file/apply_patch 等保持默认 `false`。
- `RuleSource` 在 deny/allow 规则迭代时暂时统一用 `User`；Goal 191 的多层合并
  完成后改为从 layer 中取实际 source。
- `matches_pattern` 函数已存在于 `permissions.rs`；直接复用。

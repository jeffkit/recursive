# Goal 212 — Permission: 明确 Passthrough 语义，统一 interactive 工具拦截路径

**Roadmap**: 权限系统强化  
**依赖**: Goal 201（plan mode tools opt-in，已合并）

**Design principle check**:
- 修改 `src/tools/mod.rs` — `invoke_with_audit` 逻辑调整
- 修改 `src/permissions.rs` — `Permission` 枚举重命名
- ❌ 不新增 Cargo 依赖

## Why

当前 `check_static()` 返回 `Permission::Passthrough` 时，
`invoke_with_audit()` 将其与 `Permission::Allowed(_)` 合并处理，
等效于隐式允许。这造成两个问题：

1. **语义模糊**：`Passthrough` 本意是"没有规则命中，由上层决定"，
   但实际上在 `invoke_with_audit` 中等于"放行"。
2. **双轨机制**：interactive 工具的确认逻辑分散在两处：
   - `invoke()` 层：`PermissionHook`（TUI 专用，拦截全部工具）
   - `invoke_with_audit()` 层：`is_headless + any_interactive`（仅 Headless）
   
   非 headless 的库调用方若未注册 hook，interactive 工具将无声放行。

## Scope

### 1. 重命名 `Permission::Passthrough` → `Permission::Unknown`

```rust
pub enum Permission {
    Allowed(DecisionReason),
    Denied(DecisionReason, String),
    /// 没有规则命中；由上层运行时决定是否允许。
    Unknown,
}
```

全局替换所有 `Passthrough` 引用（`permissions.rs`、`tools/mod.rs`、
测试文件）。仅改名，不改行为，确保此步骤零功能变更。

### 2. `invoke_with_audit()` 中对 `Unknown` + interactive 工具触发 hook 确认

```rust
Permission::Allowed(_) | Permission::Unknown => {
    // 若工具在 interactive 列表 且 有 hook 注册，交由 hook 决定
    if guard.any_interactive(name) {
        if let Some(hook) = &self.permission_hook {
            let preview = args_preview_for_permission(&arguments);
            if !hook.ask_permission(name, &preview).await {
                return ToolDispatch {
                    result: Err(Error::PermissionDenied {
                        name: name.into(),
                        reason: DecisionReason::Hook { name: name.into() },
                    }),
                    audit: None,
                };
            }
        }
        // 没有 hook 且非 headless → 允许（库调用方默认行为不变）
    }
}
```

**注意**：现有 `invoke()` 层已经有 hook 调用（Goal-161），
此处的改动是让 `invoke_with_audit()` 内部的逻辑也能走到 hook。
两者不冲突：`invoke()` 是外部入口检查，`invoke_with_audit()` 
是被直接调用时的安全网。

### 3. 明确 `permission_hook_enabled` 的语义文档

在 `TuiPermissionHook.ask_permission` 注释中补充：

- `enabled = false`（默认）：仅在模式需要时由上层逻辑拦截
- `enabled = true`：所有工具调用均弹出用户确认

### 4. 更新 `check_static` 文档注释

```rust
/// Returns `Permission::Unknown` when no rule in any layer explicitly
/// matches `tool_name`.  The caller (`invoke_with_audit`) treats
/// `Unknown` as "allowed" for non-interactive tools and delegates to
/// the registered `PermissionHook` for tools in the interactive list.
```

## Tests to add

1. `passthrough_renamed_to_unknown_compiles` — smoke test：确保类型可以构造
2. `unknown_interactive_tool_calls_hook` — mock hook 返回 `false`，
   验证 `invoke_with_audit` 直接调用返回 `PermissionDenied`
3. `unknown_interactive_tool_no_hook_is_allowed` — 未注册 hook 时放行
4. `unknown_non_interactive_tool_no_hook_is_allowed` — 非 interactive 工具始终放行
5. `allowed_interactive_tool_with_hook_is_not_double_checked` — 
   `Allowed` 状态（有明确规则）不触发额外 hook（避免重复询问）

## Notes

- 测试中使用 `AllowHook` / `DenyHook` mock（已有基础设施，见 `src/tools/mod.rs` 末尾）
- 此 Goal 是纯内部行为澄清，不改任何配置文件格式
- `invoke()` 层的 hook 检查（Goal-161）保持不变；本 Goal 的改动只影响
  `invoke_with_audit()` 被直接调用（绕过 `invoke()`）的场景

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets -- -D warnings` 干净
- 所有 `Passthrough` 引用全部替换为 `Unknown`（`rg Passthrough src/` 输出为空）
- interactive 工具在注册了 mock deny-hook 且被 `invoke_with_audit` 直接调用时
  返回 `PermissionDenied`

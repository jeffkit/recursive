# Goal 133 — PermissionsConfig 基础设施

**Roadmap**: Phase 17.3 — Tool Permission System (part 1/3)

**Design principle check**:
- Implemented as: new `src/permissions.rs` module + hook into `ToolRegistry::invoke()`
- ❌ Does NOT branch inside agent loop
- Purely additive; default behaviour unchanged

## Why

`PermissionHook` 类型在 `agent.rs` 中已存在多年，Agent 循环在每次工具调用前都会检查它——但没有任何人实际设置它。现在需要一个具体的、可配置的权限策略，让用户能通过配置文件和环境变量限制 Agent 可以调用哪些工具。

## Scope (do exactly this, no more)

### 1. 新建 `src/permissions.rs`

包含：
- `Permission` 枚举（`Allowed` / `Denied(String)`）
- `PermissionsConfig` 结构体，带 `serde::Deserialize`
  - `allow: Vec<String>`
  - `deny: Vec<String>`
  - `interactive: Vec<String>`
- `check_static(&self, tool_name: &str) -> Permission`：deny 优先于 allow，空列表 = 允许全部
- `is_interactive(&self, tool_name: &str) -> bool`：检查是否需要交互确认（deny 中的工具返回 false）
- 私有函数 `matches_pattern(pattern: &str, name: &str) -> bool`：支持 `"run_*"` 通配符

### 2. 挂载到 `ToolRegistry`（`src/tools/mod.rs`）

- 新增字段 `permissions: Option<PermissionsConfig>`
- 新增方法 `pub fn with_permissions(mut self, permissions: PermissionsConfig) -> Self`
- 在 `invoke()` 方法顶部插入静态权限检查：
  ```rust
  if let Some(ref perms) = self.permissions {
      match perms.check_static(name) {
          Permission::Denied(reason) => {
              return Err(Error::Tool { name: name.into(), message: reason });
          }
          Permission::Allowed => {}
      }
  }
  ```

### 3. 在 `src/lib.rs` 中导出模块

```rust
pub mod permissions;
```

### 4. Tests

`src/permissions.rs` 中 `#[cfg(test)] mod tests`：
- `test_deny_overrides_allow`
- `test_empty_allow_allows_all`
- `test_allow_list_blocks_unknown`
- `test_wildcard_matches_prefix`
- `test_wildcard_exact`
- `test_is_interactive`
- `test_default_config_allows_all`

`src/tools/mod.rs` 现有测试模块中加一个：
- `test_permission_deny_blocks_invoke`

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- 不传 permissions 时行为不变（向后兼容）

## Notes for the agent

- `Permission` 是新类型，不要和 agent.rs 中已有的 `PermissionDecision` / `PermissionHook` 混淆。
- `ToolRegistry` 新增字段时更新 `with_same_transport()` 和 `Default` 实现。
- `matches_pattern` 用 `strip_suffix('*')`，不引入 regex 依赖。
- **DO NOT modify `src/agent.rs`** — 权限在 ToolRegistry 层。
- **DO NOT modify `src/main.rs`** — CLI 标志在后续 goal 处理。

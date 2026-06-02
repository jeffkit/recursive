# Goal 190 — P1-1a: PermissionMode 枚举 + plan-mode 对接

**Roadmap**: Permission System V2 — Phase 1 基础架构

**依赖**: 无（可与 Goal 191、192 并行）

**Design principle check**:
- 修改 `src/permissions.rs`，扩展枚举
- 修改 `src/tools/plan_mode.rs`，对接新 mode
- ❌ 不修改 `agent.rs` 主循环

## Why

当前 `PermissionsConfig` 只有静态 allow/deny/interactive 三列表，无法表达
"只读探索"、"自动放行写操作"、"跳过所有确认"等运行时模式语义。plan-mode 目前
通过独立的 `AtomicBool` 控制，与权限系统正交，导致模式组合语义缺失。

## Scope

### 1. `src/permissions.rs` — 新增 `PermissionMode`

在文件顶部（`use` 块之后）添加：

```rust
/// 权限决策模式。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
    Plan {
        pre_plan_mode: Box<PermissionMode>,
        bypass_available: bool,
    },
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}
```

在 `PermissionsConfig` 结构体中新增字段：

```rust
pub struct PermissionsConfig {
    // ... 现有字段 ...
    #[serde(default)]
    pub mode: PermissionMode,
}
```

### 2. `src/tools/plan_mode.rs` — EnterPlanMode / ExitPlanMode 对接

`EnterPlanModeTool::execute()` 修改：

```rust
let current_mode = self.permissions.read().mode.clone();
let bypass_available = matches!(current_mode, PermissionMode::BypassPermissions);
let new_mode = PermissionMode::Plan {
    pre_plan_mode: Box::new(current_mode),
    bypass_available,
};
self.permissions.write().mode = new_mode;
// 保留 AtomicBool 向后兼容
self.gate.exploring_plan_mode.store(true, Ordering::Relaxed);
```

`ExitPlanModeTool::execute()` 修改：

```rust
let mut perms = self.permissions.write();
let restored = if let PermissionMode::Plan { pre_plan_mode, .. } = &perms.mode {
    *pre_plan_mode.clone()
} else {
    PermissionMode::Default
};
perms.mode = restored;
self.gate.exploring_plan_mode.store(false, Ordering::Relaxed);
```

### 3. `config.toml` 支持

`[permissions]` 段新增可选 `mode` 字段：

```toml
[permissions]
mode = "default"  # default | acceptEdits | bypassPermissions | dontAsk
```

`Plan` variant 不可通过配置文件直接设置（仅运行时进入）。

### 4. 单元测试

在 `src/permissions.rs` 的 `#[cfg(test)] mod tests` 中添加：

- `mode_default_roundtrip`: serde 序列化/反序列化 `Default`
- `mode_plan_stores_pre_mode`: 构造 `Plan { pre_plan_mode: Box::new(AcceptEdits), bypass_available: false }` 并断言字段
- `enter_exit_plan_mode_restores_mode`: 模拟 enter/exit，断言 mode 还原为原值

## Acceptance

- `cargo test --workspace` 绿色（含新测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all` 无变更
- `PermissionsConfig` 默认构造后 `mode == PermissionMode::Default`
- enter plan-mode → mode 变为 `Plan { .. }` → exit → mode 还原

## Notes for the agent

- `plan_mode.rs` 中 `EnterPlanModeTool` 和 `ExitPlanModeTool` 需要持有
  `Arc<RwLock<PermissionsConfig>>` 引用；检查现有构造方式并适配。
- `PermissionMode::Plan` 的 `Box<PermissionMode>` 是为了避免递归类型大小无限；
  serde 对 box 字段默认透明，无需额外属性。
- 不修改 `check_static()` 的行为（mode 语义集成在 Goal 191 中）。

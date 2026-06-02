# Goal 192 — P1-3: DecisionReason + Permission 枚举扩展

**Roadmap**: Permission System V2 — Phase 1 基础架构

**依赖**: 无（可与 Goal 190、191 并行）

**Design principle check**:
- 修改 `src/permissions.rs`，扩展 `Permission` 枚举
- ❌ 不修改 `agent.rs` 主循环

## Why

当前 `Permission` 枚举只有 `Allowed`/`Denied`，无法携带决策原因。调试权限问题
时无法判断是哪条规则、哪个 mode、还是哪个 hook 触发了拒绝。审计日志也无法记录
决策来源。

## Scope

### 1. `src/permissions.rs` — 新增 `DecisionReason`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionReason {
    Rule { source: RuleSource, pattern: String },
    Mode(PermissionMode),
    Hook { name: String },
    SafetyCheck { path: String },
}
```

### 2. `Permission` 枚举扩展

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Allowed(DecisionReason),
    Denied(DecisionReason, String),  // (reason, human-readable message)
    /// 工具自身未决定，由上层规则决定
    Passthrough,
}

impl Permission {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Permission::Allowed(_))
    }
    pub fn is_denied(&self) -> bool {
        matches!(self, Permission::Denied(_, _))
    }
}
```

### 3. 向后兼容

现有 `check_static()` 返回值为旧 `Permission`（假设为简单 bool 或二值枚举）；
迁移步骤：
1. 在 `check_static()` 内部，原来返回 `Allowed` 的地方改为
   `Permission::Allowed(DecisionReason::Rule { source: RuleSource::User, pattern: "...".into() })`
2. 原来返回 `Denied` 的地方补充 reason 和 message
3. 调用点只用 `is_allowed()` / `is_denied()` 的保持不变

### 4. 错误集成

在 `src/error.rs` 中新增或更新 `PermissionDenied` 变体以携带 `DecisionReason`：

```rust
PermissionDenied {
    reason: crate::permissions::DecisionReason,
    message: String,
},
```

### 5. 单元测试

- `permission_is_allowed_helper`: `Allowed(...)`.is_allowed() == true
- `permission_is_denied_helper`: `Denied(...)`.is_denied() == true
- `passthrough_is_neither`: `Passthrough.is_allowed() == false && is_denied() == false`
- `decision_reason_rule_debug`: `DecisionReason::Rule { .. }` 格式化不 panic

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `check_static()` 返回 `Permission` 带 `DecisionReason`
- `Error::PermissionDenied` 携带 `DecisionReason`

## Notes for the agent

- `DecisionReason` 引用 `RuleSource`（Goal 191）和 `PermissionMode`（Goal 190）；
  三个 Goal 并行时先定义空占位类型，合并后替换。若串行实现则按 191 → 190 → 192 顺序。
- `Passthrough` variant 为 Goal 198（工具级 check_permissions）预留，本 Goal 只需定义，
  不需要在 `check_static()` 中使用。
- `Display` 实现非必须，但 `Debug` derive 必须覆盖所有新类型。

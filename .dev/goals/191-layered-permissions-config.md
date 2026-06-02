# Goal 191 — P1-2: 多源规则分层（LayeredPermissionsConfig）

**Roadmap**: Permission System V2 — Phase 1 基础架构

**依赖**: 无（可与 Goal 190、192 并行）

**Design principle check**:
- 修改 `src/permissions.rs`，替换 `PermissionsConfig`
- 修改 `src/config_file.rs`，更新加载逻辑
- ❌ 不修改 `agent.rs` 主循环

## Why

当前只有单层全局 `PermissionsConfig`，无法区分规则来源。生产级 agent 需要
支持用户级（`~/.recursive/config.toml`）、项目级（`.recursive/config.toml`）、
会话级（运行时 API）三层规则，且各层有不同优先级和合并语义。

## Scope

### 1. `src/permissions.rs` — 新增分层类型

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum RuleSource {
    Session,   // 最高优先级
    Project,
    User,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionLayer {
    pub source: RuleSource,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub interactive: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LayeredPermissionsConfig {
    pub mode: PermissionMode,  // 依赖 Goal 190
    pub layers: Vec<PermissionLayer>,
}
```

**合并语义**（`LayeredPermissionsConfig::effective_rules()`）：
- deny：任意层 deny 即拒绝（取并集）
- allow：必须所有相关层 allow（取交集；无 allow 规则视为全放行）
- interactive：任意层标记即走交互（取并集）

### 2. `src/config_file.rs` — 分层加载

```rust
pub fn load_layered_permissions(workspace: &Path) -> LayeredPermissionsConfig {
    let mut config = LayeredPermissionsConfig::default();

    // User layer
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".recursive").join("config.toml");
        if let Ok(layer) = load_permission_layer(&path, RuleSource::User) {
            config.layers.push(layer);
        }
    }

    // Project layer
    let project_path = workspace.join(".recursive").join("config.toml");
    if let Ok(layer) = load_permission_layer(&project_path, RuleSource::Project) {
        config.layers.push(layer);
    }

    // Session layer（空，运行时填充）
    config.layers.push(PermissionLayer {
        source: RuleSource::Session,
        ..Default::default()
    });

    config
}
```

### 3. 向后兼容

`PermissionsConfig`（旧类型）保留为 `LayeredPermissionsConfig` 的别名或
转换函数，确保现有调用点编译不报错：

```rust
pub type PermissionsConfig = LayeredPermissionsConfig;

impl From<OldPermissionsConfig> for LayeredPermissionsConfig {
    fn from(old: OldPermissionsConfig) -> Self { ... }
}
```

若现有代码直接访问 `.allow`/`.deny`/`.interactive` 字段，提供委托方法：

```rust
impl LayeredPermissionsConfig {
    pub fn all_deny(&self) -> impl Iterator<Item = &str> { ... }
    pub fn all_allow(&self) -> impl Iterator<Item = &str> { ... }
    pub fn all_interactive(&self) -> impl Iterator<Item = &str> { ... }
}
```

### 4. 单元测试

- `deny_wins_across_layers`: user 层 allow，project 层 deny → 结果 Denied
- `allow_requires_all_layers`: user 层无 allow 规则，project 层 allow → 放行
- `interactive_union`: user 层和 project 层各标记不同工具 → 两者都走交互
- `session_layer_always_present`: `load_layered_permissions` 返回值包含 Session 层

## Acceptance

- `cargo test --workspace` 绿色（含新测试）
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- 现有 `check_static()` 调用点编译不报错（向后兼容）
- 加载顺序：User → Project → Session

## Notes for the agent

- `dirs` crate 已在依赖中；若无，从 `std::env::var("HOME")` 获取。
- Session 层始终存在且为空；Goal 196（运行时规则更新）将向其写入。
- `PermissionMode` 字段依赖 Goal 190；若并行实现，可先用 `#[allow(dead_code)]`
  占位，待 Goal 190 合并后激活。

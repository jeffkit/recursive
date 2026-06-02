# Goal 197 — P3-1: 运行时规则更新 API

**Roadmap**: Permission System V2 — Phase 3 运行时控制

**依赖**: Goal 191（LayeredPermissionsConfig）、Goal 193（check_static）

**Design principle check**:
- 修改 `src/permissions.rs`，添加 session 规则操作方法
- 可选：修改 `src/http.rs`，添加 `/permissions` 端点
- ❌ 不修改 `agent.rs` 主循环

## Why

当前权限规则在启动时加载，session 期间无法动态授权。用户在交互过程中
允许某个工具后，应该能在当前 session 内持续生效，无需重启。

## Scope

### 1. `src/permissions.rs` — `SharedPermissions` + 运行时 API

```rust
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedPermissions = Arc<RwLock<LayeredPermissionsConfig>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleBehavior {
    Allow,
    Deny,
    Interactive,
}

impl LayeredPermissionsConfig {
    pub fn add_session_rule(&mut self, behavior: RuleBehavior, pattern: String) {
        let session_layer = self.session_layer_mut();
        let rule = PermissionRule::parse(&pattern);
        match behavior {
            RuleBehavior::Allow => session_layer.allow.push(rule),
            RuleBehavior::Deny => session_layer.deny.push(rule),
            RuleBehavior::Interactive => session_layer.interactive.push(rule),
        }
    }

    pub fn remove_session_rule(&mut self, behavior: RuleBehavior, pattern: &str) {
        let session_layer = self.session_layer_mut();
        let list = match behavior {
            RuleBehavior::Allow => &mut session_layer.allow,
            RuleBehavior::Deny => &mut session_layer.deny,
            RuleBehavior::Interactive => &mut session_layer.interactive,
        };
        list.retain(|r| r.tool_name != PermissionRule::parse(pattern).tool_name
            || r.content_pattern != PermissionRule::parse(pattern).content_pattern);
    }

    pub fn session_rules(&self) -> &PermissionLayer {
        self.layers.iter()
            .find(|l| l.source == RuleSource::Session)
            .expect("session layer always present")
    }

    fn session_layer_mut(&mut self) -> &mut PermissionLayer {
        self.layers.iter_mut()
            .find(|l| l.source == RuleSource::Session)
            .expect("session layer always present")
    }
}
```

### 2. （可选）HTTP 端点集成

若 `src/http.rs` 已有 session 管理，添加：

```
POST /sessions/{id}/permissions
Content-Type: application/json
{ "action": "add", "behavior": "allow", "pattern": "shell(git *)" }

DELETE /sessions/{id}/permissions
Content-Type: application/json
{ "behavior": "deny", "pattern": "shell(rm *)" }
```

Handler 获取对应 session 的 `SharedPermissions`，调用
`add_session_rule` / `remove_session_rule`。

若 HTTP server 尚未实现 session 权限管理，本 Goal 仅实现 `permissions.rs`
的 API，HTTP 集成标记为 TODO。

### 3. `SharedPermissions` 传播

在 `main.rs` / agent 初始化处，将 `PermissionsConfig` 包装为
`Arc<RwLock<...>>`，并传递给 `ToolRegistry`、plan_mode 工具等使用权限的
所有组件。

### 4. 单元测试

- `add_session_allow_rule`: 添加 allow 规则后 `session_rules().allow` 包含该规则
- `remove_session_rule`: 添加再删除后 `allow` 为空
- `session_rule_takes_precedence`: session deny 覆盖 user allow（deny 取并集）
- `shared_permissions_arc`: 两个 Arc::clone 共享同一底层，写操作对两方可见

## Acceptance

- `cargo test --workspace` 绿色
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- `add_session_rule` 在不重启的情况下立即生效于后续 `check_static` 调用
- `SharedPermissions` 类型别名在需要权限的组件间正确传递

## Notes for the agent

- `Arc<RwLock<LayeredPermissionsConfig>>` 在 `ToolRegistry::invoke()` 里
  需要 `read()` 获取快照再做检查，避免长时间持锁。
- HTTP 端点是可选的；若工期紧，仅实现 `permissions.rs` 方法并记录 TODO。
- 运行时规则**不持久化**（仅内存，重启清空）——这是设计决策，符合提案范围。

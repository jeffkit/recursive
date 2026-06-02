# Goal 179 — Dynamic Team Management: `team_add_role` / `team_remove_role` Tools

**Roadmap**: Phase 18 — Advanced Agent Patterns (coordinator pattern)
**Design principle check**:
- 新工具文件 `src/tools/team_manage.rs`，注册到 `src/tools/mod.rs`
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支
- ✅ 纯新增能力

## Why

当前 `AgentPool` 的角色必须**预先静态注册**，无法在运行时添加或删除。
Fake CC 的 `TeamCreateTool` / `TeamDeleteTool` 允许 coordinator 动态组建和解散团队。

动态 Team 管理的价值：
- 让主 Agent 根据任务性质决定需要哪些专家
- 支持按需创建一次性专家（如「为这个任务创建一个 SQL 专家」）
- 在复杂工作流中动态调整团队组成

## What this goal does

### 1. 共享 AgentPool (`Arc<RwLock<AgentPool>>`)

为让工具能够修改 `AgentPool`，需要引入 `Arc<tokio::sync::RwLock<AgentPool>>`。

### 2. 新工具 `team_add_role`

参数：
```json
{
  "name": "string (required) — 角色名称",
  "system_prompt": "string (required) — 该角色的系统提示词",
  "max_steps": "integer (default 30) — 最大步骤数",
  "allowed_tools": "array of string (optional) — 工具白名单，空则全部可用"
}
```

行为：向共享 AgentPool 注册一个新角色。如果角色名已存在，覆盖旧定义。
返回：`"Role '{name}' added to team pool."` 或错误信息。

### 3. 新工具 `team_remove_role`

参数：
```json
{
  "name": "string (required) — 要删除的角色名称"
}
```

行为：从共享 AgentPool 删除该角色。
返回：`"Role '{name}' removed."` 或 `"Role '{name}' not found."`.

### 4. 新工具 `team_list_roles`

参数：无

行为：列出当前 AgentPool 中所有角色。
返回：角色名列表（换行分隔）。

### 5. 工具结构

- `TeamManageTools` 包含一个 `Arc<RwLock<AgentPool>>`
- 由三个独立工具结构实现：`TeamAddRole`, `TeamRemoveRole`, `TeamListRoles`

## Files to change

| File | Change |
|------|--------|
| `src/tools/team_manage.rs` (new) | 三个工具 + 测试 |
| `src/tools/mod.rs` | `pub mod team_manage;` + 注册导出 |
| `src/multi.rs` | 为 `AgentPool` 添加 `remove_role` 方法 |

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy -- -D warnings` 干净
3. 三个新工具通过 4+ 单元测试

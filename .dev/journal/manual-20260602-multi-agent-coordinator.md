# Manual edit: multi-agent coordinator pattern improvements

**Date**: 2026-06-02
**Goal**: 提升多 Agent 协同能力，向 Fake CC coordinator 模式对齐
**Files touched**:
- `src/multi.rs` — `AgentRole` + `AgentPool` 派生 `Clone`；`AgentPool` 新增 `remove_role` 方法；`TeamOrchestrator::run` 改为并行执行委派任务（`tokio::task::JoinSet`）
- `src/tools/spawn_worker.rs` (new) — `SpawnWorkerTool`：first-class 委派工具，支持 5 种预设 worker 类型
- `src/tools/team_manage.rs` (new) — `TeamAddRole` / `TeamRemoveRole` / `TeamListRoles`：动态 Team 管理工具
- `src/tools/send_message.rs` (new) — `WorkerMailbox` / `WorkerRegistry` / `SendMessageTool`：双向通信基础设施
- `src/tools/mod.rs` — 注册以上所有新模块和导出
- `src/main.rs` — 在 sub_agent 启用时同步注册 `SpawnWorkerTool`
- `.dev/goals/177-parallel-team-orchestrator.md` (new)
- `.dev/goals/178-spawn-worker-tool.md` (new)
- `.dev/goals/179-dynamic-team-management.md` (new)
- `.dev/goals/180-send-message-tool.md` (new)

**Tests added**:
- `spawn_worker`: 4 tests
- `team_manage`: 6 tests
- `send_message`: 6 tests (包含 mailbox、registry、tool 测试)
- 总计新增 16 个测试，全部绿灯

**Quality gates**:
- `cargo test --workspace`: ✅ 全部通过（902+ tests in lib，+整合测试集）
- `cargo clippy --all-targets --all-features -- -D warnings`: ✅ 无警告无错误
- `cargo fmt --all`: ✅

**Notes**:
- `TeamOrchestrator` 并行化：使用 `Arc<AgentPool>` 克隆（内部 SharedMemory/MessageBus 均为 Arc，共享）
- `spawn_worker` vs `sub_agent`：`sub_agent` 保持向后兼容，`spawn_worker` 面向 coordinator 模式，更丰富的角色配置
- `send_message` 的 kernel 集成（mailbox 接入 AgentKernel turn loop）留作下一步，Goal 180 文件已说明
- A2A (Goal 176) 的 `a2a.rs` + `mod.rs` 修改仍在工作区未提交，本次未触碰

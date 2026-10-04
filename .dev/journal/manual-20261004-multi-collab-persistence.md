# manual-20261004-multi-collab-persistence

## Date
2026-10-04

## Goal
#106 feat(multi): 多智能体协作状态纯内存——MessageBus 1000 条 ring、
SharedMemory/WorkerMailbox 不落盘，无 artifact handoff

## Files touched
- `src/multi.rs`（`SharedMemory` / `MessageBus` 写穿持久化 + 合并式 restore；
  `AgentPool::with_backend` / `restore` / `ensure_restored`；
  `register_subagent_if_enabled` 注入带 backend 的 pool + artifact store，
  并向 **coordinator** 注册 `artifact_read` / `artifact_list`）
- `src/tools/artifacts.rs`（新增：artifact 交接协议——`ArtifactStore` +
  `artifact_read` / `artifact_list` 工具）
- `src/tools/agent.rs`（worker 终稿落 artifact 并回传 id+预览引用；
  `execute` 首次派发时惰性 rehydrate pool / artifact index）
- `src/tools/mod.rs`（注册 `artifacts` 模块与再导出）

## 拍板内容
- **持久化走既有 `StorageBackend` trait**（本地 → `LocalStorageBackend`，
  云 → S3/Redis，同 cloud-runtime 配套），key 命名空间 `multi/*`：
  `multi/shared-memory.json`、`multi/message-bus.json`、
  `multi/artifacts/<id>` + `multi/artifacts/index.json`。
- **写穿 + 合并 restore**：in-memory 始终是热路径与真相；每次变更 best-effort
  落盘（失败只 `tracing::warn`，绝不打断协作）。bus 的持久化写入与既有快照
  **按 id 去重合并**（两副本/重启后先写再 restore 都不会互相丢消息），
  `clear()` 落空快照而非合并（避免重启复活旧消息）。restore 时活数据优先。
- **artifact 交接**：worker 终稿全文落 `multi/artifacts/<id>`，返回块只带
  id + name + bytes + ≤2KiB 头/尾预览 + 「用 `artifact_read` 取全文」指令；
  小产物（≤2KiB）仍全文内联，不退化小结果的可读性。消费方按需 `artifact_read`。
  落盘失败时**回退为旧的全文内联**结果（绝不谎报已保存）。
- **lazy rehydrate**：`register_subagent_if_enabled` 是同步函数（TUI
  `build_runtime` 等同步入口会调），不能在内部 await；故 pool / artifact index
  的 restore 由 `AgentTool::execute` 首次派发时经 `ensure_restored()`（单发
  `AtomicBool`）触发。
- **WorkerMailbox 有意不落盘**：注册表持有的是活 worker 的投递句柄，进程重启后
  worker 已死，复活陈旧 mailbox 只会误投，故不持久化。team roster 路径
  （`RECURSIVE_TEAMS_DIR` / `~/.claude/teams`）为 spec 既定，未改。

## Tests added
- `src/multi.rs`
  - `shared_memory_persists_and_restores_across_instances`
  - `shared_memory_restore_never_clobbers_live_entries`
  - `shared_memory_seq_stays_monotonic_after_restore`
  - `shared_memory_persist_failure_degrades_not_errors`
  - `shared_memory_remove_persists_tombstone`
  - `message_bus_history_persists_and_restores_across_instances`
  - `message_bus_restore_respects_capacity_and_live_writes`
  - `message_bus_clear_persists_empty_history`
  - `agent_pool_restore_rehydrates_memory_and_bus`
  - `agent_pool_ensure_restored_is_single_shot_and_keeps_state`
  - `register_subagent_if_enabled_registers_artifact_tools`（coordinator 侧
    必须能看到 `agent` / `artifact_read` / `artifact_list`，否则 worker 结果里
    广告的 `artifact_read` 不可调用）
- `src/tools/artifacts.rs`
  - `put_and_get_round_trip` / `get_unknown_id_is_not_found` /
    `get_rejects_path_traversal`（id 防穿越）
  - `artifacts_survive_a_fresh_store_via_restore` /
    `ensure_restored_rehydrates_index_once`
  - `list_tracks_live_writes_without_restore`
  - `reference_text_small_body_is_inlined_in_full` /
    `reference_text_large_body_is_head_and_tail_preview`
  - `put_failure_propagates_not_advertised`（失败必须 `Err`，宁可回退内联也别
    谎报）
  - `artifact_read_tool_executes` / `artifact_read_tool_missing_id_errors` /
    `artifact_read_tool_readonly` / `artifact_list_tool_empty_and_nonempty` /
    `artifact_list_tool_rehydrates_index_on_a_fresh_process`

## Verification
- `cargo test -p recursive-agent --lib`：2510 passed（唯一失败
  `tools::execution::shell::tests::timeout_kills_child_process` 为高负载下的
  时序 flake，单独重跑通过）
- 目标用例 62/62 通过（`multi::tests::` + `tools::artifacts::tests::`）
- `cargo test -p recursive-agent --test issue407-cancel-surfaces`：3/3 通过
  （enabled `register_subagent_if_enabled` 路径——新增 artifact 工具 + 带
  backend 的 pool 不破坏既有取消/配对行为）
- `cargo clippy -p recursive-agent --all-targets --all-features -- -D warnings`
  干净
- `cargo fmt --all -- --check` 干净

## Notes
- 无新增依赖（复用 `blake3` / `serde_json` / `tempfile` 与既有 `StorageBackend`）。
- `register_subagent_if_enabled` 之前**根本没有 pool**（只在此处经
  `with_pool` 注入 AgentTool），即 coordinator 路径上 shared memory / bus /
  artifact 工具此前从未接通；本次一并接上。

# manual-20261003-agui-per-request-system-prompt

## Date
2026-10-03

## Goal
#68 — `/agui` per-request system prompt（RunAgentInput 无 prompt 字段、`forwardedProps`/`state`
解析后即丢弃，AG-UI 成为唯一不支持按请求下发 prompt 的通道）。

本 worktree（`v2-pipeline-68-1003001814-cont`）继承的是上一轮 pipeline 的半成品
（`820bef10` 已完成主体修复、`858f12b8` 为带毒 WIP 快照）。本轮任务是**评估、修复、收口**，
而非重写。

## Files touched
- `src/paths.rs`（本轮唯一改动，+14/−16）

## What was already in place（820bef10，经审查保留）
- `agui-protocol::RunAgentInput` 新增可选 `systemPrompt` / `appendSystemPrompt`
  （serde default + `skip_serializing_if`，camelCase；旧 payload 序列化逐字节不变，有测试钉住）。
- `handlers::agui_run` 回落链：显式 `systemPrompt` > `forwardedProps.systemPrompt` >
  `state.systemPrompt` > 进程级 `state.config.system_prompt`；`appendSystemPrompt`
  三处同源、追加而非替换（与 `/run`、`/sessions` 语义一致）。
  `assemble_system_prompt` 仍在其上叠 project context / skills，服务端段落不可被覆盖；
  并补齐了 `/agui` 缺失的 `inject_environment_segment` 对齐（#31 §2）。
- 验收测试：handlers.rs 7 个（override 三槽位、回落、append、双线程隔离、服务端段落存活、
  协议向后兼容）+ `tests/agui_e2e.rs` 1 个端到端隔离测试。

## What this round fixed
`858f12b8`（WIP 快照）在
`paths::tests::user_sessions_dir_creates_dir_when_absent` 里，于
`PinnedRecursiveHome::new(...)`（**已持有** 全局 env 锁）之后又调了一次
`crate::test_util::env_lock()`。std Mutex 不可重入 → 该测试在并行竞争下一上来就自锁。

这正是 #68 管线连续 5 次 `engine_error`（executor 7200s 超时）的根因：`sample` 显示
10+ 个测试线程全部卡在 `_pthread_mutex_firstfit_lock_slow`（`env_lock`），lib 测试永不结束。
单独运行该测试 + `--test-threads=1` 也复现挂死（自锁与并发无关，与是否有第二个等待者有关）。

修复：删除冗余的二次加锁与手工 save/restore —— guard 的语义（pin 住
`RECURSIVE_SESSIONS_DIR`）就是本测试需要的全部隔离；并新增断言
`sessions.starts_with(home.path())` 把「必须解析到 pinned home 之下」钉进测试。

## Tests added
- `paths::tests::user_sessions_dir_creates_dir_when_absent` 内新增
  pinned-home 归属断言（修复自锁的同时收紧不变量）。

## Gates
- `cargo test --workspace`：全绿（lib 2483 passed；连续两轮 lib 复跑稳定，无 SIGKILL/挂死）。
- `cargo clippy --all-targets --all-features -- -D warnings`：clean。
- `cargo fmt --all`：已应用。

## Notes
- #68 的验收清单逐条对照：覆盖生效/回落（1a–1d）、append（1e）、同进程双 threadId
  隔离（2，unit+e2e 双钉）、skills/项目上下文不被覆盖（3）、旧客户端逐字节兼容（协议测试）——全部有测试。
- 与 #62 的接缝保持原样：`prepare_run`（历史装配）与 prompt 装配仍分离，未越权合并。
- 教训已口头总结：**「guard 已持锁」的工具函数（PinnedRecursiveHome / IsolatedWorkspace /
  SessionEnvPins）绝不能在同一作用域再取 env_lock**——值得后续在 `.dev/AGENTS.md`
  的 env-var 测试条目下补一句（本轮不动主仓库文档，避免范围蔓延）。

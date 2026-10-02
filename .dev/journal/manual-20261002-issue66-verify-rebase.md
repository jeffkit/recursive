# manual-20261002-issue66-verification-rebase

- **Date**: 2026-10-02
- **Goal**: issue #66 验证收尾——上一次 run（pipeline-66-1001232353，终端态
  engine_error）的产品实现已完整落在本分支 WIP commit 上，但分支基线落后 main
  33 个 commit，在 keeper 环境（`RECURSIVE_SESSIONS_DIR` 指向管线共享目录）
  下 `cargo test --workspace` 有 7 个失败。本次：验证实现 → rebase → 修复
  rebase 冲突与暴露的隔离缺口 → 全门禁跑绿。

## What was verified (existing implementation, no product change needed)

- **§3.2 流式**：`build_session_runtime`（`src/http/handlers.rs:123`）对全部
  HTTP 入口（/sessions、/run、/agui）开 `.streaming(true)`；RunCore 的
  partial-token forwarder 由此激活。`AguiConverter` 新增 `open_accumulated`：
  `AssistantText` 到达时 exact-match 只发 `TextMessageEnd`、前缀-remainder 只
  补发尾部、分歧回退整段消息——不重不漏（3 个 converter 单测 + 1 个端到端
  分块 provider 测试钉住 ≥2 帧、拼接恰好一次、RunFinished 收尾）。
- **§3.3 取消**：
  - A（断流）：`CancelOnDrop<S>` 包装 SSE body，drop 即 cancel
    `CancellationToken`；30s keep-alive 压低断流感知延迟。
  - B（显式）：`POST /agui/{thread_id}/cancel`（幂等 200/`cancelled:false`），
    `AppState.agui_active_runs` 注册表（AG-UI run 无 SessionState 行，不能
    复用 per-session interrupt_token）。
  - token 经 `runtime.set_interrupt_token` 进入 kernel：步骤边界
    `check_shutdown` + LLM 调用中段 select（`complete_with_budget` /
    stream cancel_token）→ `FinishReason::Cancelled`。取消的 run 记
    `record_run_failed`、RunFinished 带 Error outcome `code:"cancelled"`；
    admission permit 移入 driver task（修了 handler 返回即释放的假约束）。
  - `agui-client` 补 `cancel(thread_id)`；`agui-tui` 退出时调用。
- `MockProvider::with_stream_chunk_chars(n)`（char 边界安全分块）。

## What this session fixed

1. **rebase main（fac622dc）**：`src/http/handlers.rs` 两处冲突——
   driver-task 捕获段（main 的 #57 `drv_model/drv_pre_run_len` × #66 的
   `drv_state/drv_thread_key` 合并保留双方）与 driver 收尾段（#57 的
   `drop(run_guard)` per-thread 栅栏 × #66 的 cancel-registry 清理，两个都
   保留、先 cancel-registry 后 run_guard）。其余 11 个文件干净合入。
2. **`src/http/mod.rs`**：goal_396 测试的 `AppState` 字面量缺
   `agui_active_runs`（rebase 后新 E0063）——补字段。
3. **`tests/agui_e2e.rs`**：`HomeOverride` 补 `RECURSIVE_SESSIONS_DIR` pin
   （Goal-H J1 硬覆盖压过 RECURSIVE_HOME；keeper 环境继承值把 persist_run
   重定向进共享根，native-session 测试数到别人的 14 条消息、状态读到
   Completed/Interrupted 错位）。同因 main 已在 resume_by_id / test_util 修过，
   本文件是 #57 新增测试时漏掉的同型缺口。

## Gates (keeper env `RECURSIVE_SESSIONS_DIR` 设定 + unset 双跑)

- `cargo test --workspace`：**3813 passed / 0 failed**（58 个套件全绿）
- `cargo clippy --all-targets --all-features -- -D warnings`：clean
- `cargo fmt --all`：已应用

## Notes

- 分支 `v2-pipeline-66-1002043154-cont`，HEAD `182d5f94` =
  rebase 后的 WIP（da0aa94c，含全部 #66 实现）+ 本次修复（182d5f94）。
- acceptance 对照：流式多帧 ✅（e2e 断言 content_frames ≥ 2 且 delta 拼接
  等于最终文本）；断流取消 ✅（drop wrapper → token cancelled 单测 +
  `agui_sse_drop_cancels_run_token`）；显式取消 ✅（端点 + 幂等单测 +
  client SDK + TUI 接线）；SDK 回归：`partial_message`/`stream_event` 在
  两个 SDK 中是 fire-hose-only，最终聚合走完整 `message` 事件，流式开启不
  影响多轮示例（`build_session_runtime` 注释有记录）。
- 遗留（非本 issue）：`tests/http_common/mod.rs::mock_config` 的
  `workspace: /tmp` 使 http.rs 的 /agui oneshot 测试在 keeper 环境下仍会向
  共享 sessions 根写 transcript（无断言受影响，仅污染）；如后续出现同类
  失败，按 agui_e2e 的 HomeOverride 模式补 pin。

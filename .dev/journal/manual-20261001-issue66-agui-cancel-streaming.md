# manual-20261001-issue66-agui-cancel-streaming

- **Date**: 2026-10-01
- **Goal**: issue #66 — AG-UI 通道补齐 token 级流式（§3.2）+ 断流/显式取消（§3.3）。
  okguitar 的端到端实测（`curl --max-time 3` 断流后 run 继续跑满 90s、transcript 15 行）
  确认 `agui_run` 的 `tokio::spawn` 驱动与 SSE 响应完全解耦，是静默成本泄漏。

## Files touched

- `src/http/mod.rs` — `AppState.agui_active_runs`（thread_id → CancellationToken 注册表，
  AG-UI run 无 SessionState 行不能复用 per-session interrupt_token）；路由
  `POST /agui/{thread_id}/cancel`；OpenAPI 补条目。
- `src/http/handlers.rs` —
  - `agui_run`：每 run 建 cancellation token，`runtime.set_interrupt_token` 安装
    （kernel 在步骤边界与 LLM 调用中段各有一处检查 → `FinishReason::Cancelled`），
    注册进 registry，driver 结束时移除；
  - `CancelOnDrop<S>`：SSE body 包装器，drop（客户端断流，hyper 在下次写时感知，
    keep-alive 把感知延迟压到 ≤30s）即 cancel token；正常完成也 drop，但 token
    已 fire，no-op；
  - 30s `keep_alive`（axum 内建 idle comment；不用 merge 心跳 interval——那会让
    流在 RunFinished 后永不结束）；
  - driver：cancel 的 run 记 `record_run_failed`、RunFinished 带/Error
    outcome `code:"cancelled"`（不再伪装 Success）；admission permit 移入 driver
    task——原先 handler 返回即释放，实际并发根本没被信号量约束（`runs_in_flight`
    也不真）；
  - `.streaming(true)`（§3.2）：RunCore 的 partial-token forwarder 原先从不启动，
    AG-UI 答案整段一帧；
  - `AguiConverter`：新增 `open_accumulated` 累计已流出的 delta；`AssistantText`
    到达时 exact-match 只发 `TextMessageEnd`，前缀-remainder 只补发尾部，
    分歧（异常 provider）回退旧的整段消息，保证不重不丢。
- `src/llm/mock.rs` — `MockProvider::with_stream_chunk_chars(n)`：流式测试可断言多帧
  投递（按 char 边界切块，CJK 安全）；默认单 chunk 行为不变。
- `crates/agui-client/src/lib.rs` — `AguiClient::cancel(thread_id)` + `CancelInfo`
  （URL 拼 path 段，幂等语义照服务端）。
- tests：`tests/http.rs` ×10、`tests/http_common/mod.rs` ×3、`tests/agui_e2e.rs`、
  `tests/v050_integration.rs`、`src/http/cold_load.rs`、handlers 测试内 AppState
  字面量补 `agui_active_runs` 字段。

## Tests added

- `agui_streamed_step_finalises_without_duplicating_text`（converter：exact 只 End、
  remainder 只补尾；CJK 多字节）
- `agui_divergent_final_text_falls_back_to_full_message`（分歧回退）
- `agui_cancel_cancels_registered_thread_and_is_idempotent`（cancel 端点 + 幂等）
- `agui_sse_drop_cancels_run_token`（断流 → token cancelled；wrapper 转发正常）
- `agui_streams_token_deltas_without_duplicating_final_text`（端到端：分块 provider →
  ≥2 个 content 帧、delta 拼接恰好一次、RunFinished 收尾）
- `stream_chunks_content_when_configured`（mock 分块，含 char 边界）

## Notes

- **双 agent 撞车**：issue-keeper 对同一评论重复派发了两个 agent（12:11 与 12:17），
  两者在同一 worktree 并发改码。12:17 的实例（本记录）发现后终止了 12:11 的实例，
  合并双方产物（保留其 agui_cancel/驱动清理/取消 outcome 实现，保留本文的
  converter 去重与 keep_alive/指标语义，去重了各自的 token-setup 与 guard 结构体）。
- 流式帧形状变化：AG-UI 客户端现在收到 Start/Content×N/End 而不是
  Start/Content×1(整段)/End；按 AG-UI 规范 delta 语义消费的客户端不受影响，
  自己拼整段的客户端也兼容（delta 拼接 == 原整段文本）。
- e2e（aimock replay）无 AG-UI 套件，不受影响。

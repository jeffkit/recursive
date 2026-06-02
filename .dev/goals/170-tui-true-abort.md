# Goal 170 — TUI: 真正的取消 LLM 请求（abort handle）

**Roadmap**: TUI 体验提升系列 — gap doc §8 ROI #4

**Design principle check**:
- 仅改 `src/tui/backend.rs`，不动核心库 / agent.rs / runtime.rs
- 利用 tokio `JoinHandle::abort()` — `_worker` 已经是 `JoinHandle<()>`，
  可在 cancel 时直接 abort 掉正在飞的 turn task
- 不改 `wait_for_cancel` 的 API（保留向下兼容），只改触发路径
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支

## Why

当前 `cancel_flag` + `select!` 的方案在每 100ms 检查一次 flag，但
reqwest 的 SSE 流仍然在继续消耗网络连接和计算资源，直到当前 token 边界
才能中断。对长推理（Claude Sonnet thinking、DeepSeek R1）来说，用户按
Esc 后可能还要等 5-30 秒才能中断，体验很差。

Gap doc §8 注明：
> 真正的取消正在飞的 LLM 请求（中 / 中）
> 落地路径：reqwest 的 RequestBuilder 在异步任务里 spawn 后保留 abort 句柄；
> `backend.rs::wait_for_cancel` 改为直接 abort handle。

## Scope

### 1. 把 turn 逻辑提取到一个可 abort 的 task

`worker_loop`（`src/tui/backend.rs`）目前在处理 `UserAction::SubmitPrompt`
/ `Bash` / `Compact` 时，直接 `tokio::select! { result = run_turn(...), _ = wait_for_cancel(...) }`.

改法：

```rust
// 伪代码示意
let turn_task: JoinHandle<Result<()>> = tokio::spawn(run_turn(rt, action, sink));
tokio::select! {
    res = &mut turn_task => { /* normal completion */ }
    _ = wait_for_cancel(cancel_flag.clone()) => {
        turn_task.abort();
        let _ = turn_task.await; // drain
        sink.send(UiEvent::TurnAborted).await;
    }
}
```

具体步骤：
1. 在 `worker_loop` 中，每次开始一个 turn，`tokio::spawn` 出 `JoinHandle`
2. `tokio::select!` 在 `JoinHandle` 完成 vs `wait_for_cancel` 之间竞争
3. cancel 获胜时调 `handle.abort()`，等 join（`.await` 会返回
   `Err(JoinError::Cancelled)`，ignore 掉）
4. 向 UI 推送一个 `UiEvent::Interrupted` 事件（已有，`src/tui/events.rs`
   应该已有此变体），让 transcript 加一行"[Interrupted]"

### 2. 处理 transcript 一致性

abort 后，若 LLM 已返回部分 `tool_call` 但对应 `tool_result` 还没回来，
下一次 turn 发给 LLM 的 messages 里会出现悬空的 `tool_call` —— OpenAI
/ Anthropic 都会 400。

解决方案：在 abort 后，让 `AgentRuntime` 的 transcript 截断到最后一条
完整 `user` 消息（即 turn 的起始点）。

做法：在 turn 开始前记录 transcript 长度 `pre_turn_len`，abort 后截断：
```rust
// 如果 Runtime 暴露了 transcript 长度 / truncate 方法，调它
// 否则，直接构建一个新的 runtime（re-use `app.runtime`，丢弃当前 turn 状态）
```

注意：`AgentRuntime` 已经在 backend 里持有；可以在 abort 后在下一个
`UserAction` 触发时，重新构建 `AgentRuntime`（传入截断后的 transcript）。
**简化方案**：abort 后推 `UiEvent::Interrupted`，同时 backend 内部把
`runtime` 标记为"需要重建"；下次用户提交时用现有 messages（截至上次
完整 user turn）重建。

### 3. UI 展示

`UiEvent::Interrupted` 已经在 `src/tui/events.rs` 里（Goal 147 加的），
app 的 `apply_event` 会把它渲染成 transcript 里的灰色"[interrupted]"块。
不需要新增变体。

### 4. 测试

在 `src/tui/backend.rs` 的 `#[cfg(test)] mod tests` 里增加：
- `abort_cancels_task_immediately`：创建一个 sleep 很长的 task，
  cancel_flag 设 true，验证 select! 返回后 task 确实被 abort 了
- `abort_after_partial_token`：mock runtime 发几个 partial token，然后
  cancel，验证 UI 收到 `TurnAborted` / `Interrupted` 事件

## Acceptance

- `cargo test` 绿
- `cargo clippy --all-targets --all-features -- -D warnings` 干净
- Esc/Ctrl+C 后，reqwest connection 在 < 200ms 内断开（不是等 token 边界）
- 下一次 turn 正常发出，没有 400 错误（transcript 一致性保证）
- `UiEvent::Interrupted` 在 transcript 可见

## Notes for the agent

- `_worker` 字段是 `JoinHandle<()>`，但它是整个 worker loop，不能 abort。
  需要在 loop 内部对每个 turn spawned 的 task 做 abort。
- `wait_for_cancel` 保持不变，继续作为 select! 的对端条件。
- reqwest 的 `Response::bytes_stream()` 在 task abort 时会被 drop，
  底层 TCP 连接随之关闭 —— 不需要额外的 `reqwest::Client::abort()`。
- 截断 transcript 最安全的方式：`AgentRuntime` 持有 `messages: Vec<Message>`，
  abort 后重新 clone runtime 但只保留 pre_turn messages。参考
  `src/runtime.rs` 的 `messages` 字段。
- **DO NOT modify** `src/agent.rs` / `src/runtime.rs` / `src/llm/`.

# Goal 170 — TUI: 真正的取消 LLM 请求（backend abort）

**Roadmap**: TUI 体验提升系列 — gap doc §8 ROI #4

**Design principle check**:
- 仅改 `src/tui/backend.rs`，UI 侧（`UiEvent::Interrupted` + handler）已由
  partial commit c17b732 完成
- 利用 `tokio::task::JoinHandle::abort()` 在取消时立即 drop reqwest 响应
- ❌ 不在 `agent.rs::Agent::run` 主循环里加分支

## Why

当前 `cancel_flag` + `select!` 方案在 flag 设为 true 后，reqwest SSE 流
仍然继续，直到 100ms poll 间隔或下一个 yield 点才中断。对长推理（Sonnet
thinking、DeepSeek R1）用户按 Esc 后可能还要等 5-30 秒。

`UiEvent::Interrupted` 和 app 里的 handler 已经存在（c17b732）。
本 goal **只改 `src/tui/backend.rs`**，把 4 个 turn path 升级为真正的 abort。

## 技术方案

`AgentRuntime::run()` 是 `&mut self`，不能直接 move 进 `tokio::spawn`。
解法：在 worker_loop 进入每次 turn 之前，把 runtime 从 `state` 里暂时取出，
包进 `Arc<tokio::sync::Mutex<Box<AgentRuntime>>>`，再 spawn。

### 具体步骤

**Step 1**: 在 worker_loop 开头或第一次使用前，把 `state` 改成持有
`Arc<tokio::sync::Mutex<Box<AgentRuntime>>>` 而不是直接持有 `Box<AgentRuntime>`：

```rust
// 在 worker_loop 初始化阶段：
let rt_lock: Arc<tokio::sync::Mutex<Option<Box<AgentRuntime>>>> = match state {
    RuntimeBuild::Ready(rt) => Arc::new(tokio::sync::Mutex::new(Some(rt))),
    RuntimeBuild::Offline { reason } => {
        // handle offline separately — offline path 不需要 spawn
        // ...
    }
};
```

实际上，最小改动方案是**不改 state 结构体**，而是在每个 turn path 内部临时
操作：

```rust
// 以 SendMessage 为例：
UserAction::SendMessage(text) => match &mut state {
    RuntimeBuild::Ready(rt_box) => {
        // 1. 取出 runtime
        let mut rt = std::mem::replace(rt_box, placeholder_rt());
        let pre_turn_len = rt.transcript().len();

        // 2. 放进 Mutex，spawn
        let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
        let rt_for_task = rt_shared.clone();
        let handle = tokio::task::spawn(async move {
            let mut guard = rt_for_task.lock().await;
            guard.run(text).await
        });

        // 3. select!
        cancel_flag.store(false, Ordering::SeqCst);
        let cancel_for_select = cancel_flag.clone();
        let result = tokio::select! {
            r = handle => match r {
                Ok(inner) => inner.map(|_| ()),
                Err(e) if e.is_cancelled() => Ok(()),
                Err(e) => Err(crate::Error::Other(e.to_string())),
            },
            _ = wait_for_cancel(cancel_for_select) => {
                // abort 会 drop reqwest 响应
                handle.abort();
                let _ = handle.await; // drain
                let _ = event_tx.send(UiEvent::Interrupted);
                Ok(())
            }
        };

        // 4. 取回 runtime（abort 后 lock 会立即可得，因为任务已 drop）
        let recovered_rt = Arc::try_unwrap(rt_shared)
            .expect("no other owners after task abort/complete")
            .into_inner();

        // 5. 若 abort，截断 transcript 到 pre-turn
        let mut recovered_rt = if result.is_ok() && /* was interrupted */ false {
            // 截断在 abort 分支里已处理，recovered_rt 仍是 pre-turn 状态
            recovered_rt
        } else {
            recovered_rt
        };
        // 重新放回 state
        *rt_box = recovered_rt;

        if let Err(e) = result { ... }
        let _ = event_tx.send(UiEvent::TurnFinished);
        cancel_flag.store(false, Ordering::SeqCst);
    }
    ...
}
```

**`placeholder_rt()`** 问题：`std::mem::replace` 需要一个临时值，但
`AgentRuntime` 没有 `Default`。解法：用 `Option<Box<AgentRuntime>>` 作为
state 中的 inner type：把 `RuntimeBuild::Ready(Box<AgentRuntime>)` 改成
`RuntimeBuild::Ready(Option<Box<AgentRuntime>>)` 并在所有 match arm 里用
`.as_mut().unwrap()` 或 `.take()` / `.replace(...)` 操作。

**最小侵入的替代方案**（推荐实现）：

用一个 `fn run_turn_with_abort(...)` 异步辅助函数，接受 `&mut AgentRuntime`
的引用，用 `unsafe` 延长生命周期（**不推荐**）；

OR：

接受 `Box<AgentRuntime>` 所有权，执行 turn，返回 `(Box<AgentRuntime>, Result<...>)`：

```rust
async fn run_turn_abortable(
    rt: Box<AgentRuntime>,
    run_fn: impl FnOnce(&mut AgentRuntime) -> impl Future<Output = Result<RuntimeOutcome>>,
    cancel_flag: Arc<AtomicBool>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
) -> (Box<AgentRuntime>, bool /* was_aborted */) {
    let pre_turn_len = rt.transcript().len();
    let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
    let rt_for_task = rt_shared.clone();
    let handle = tokio::task::spawn(async move {
        let mut g = rt_for_task.lock().await;
        run_fn(&mut *g).await
    });
    // ... select! + abort ...
    let rt = Arc::try_unwrap(rt_shared).unwrap().into_inner();
    (rt, was_aborted)
}
```

实际上 `run_fn: impl FnOnce(&mut AgentRuntime) -> impl Future` 在 Rust
里难以表达（closure 借用问题）。

**最实际的方案**（已经过思考，直接实现这个）：

把 `RuntimeBuild` 改成：

```rust
pub enum RuntimeBuild {
    Ready(Option<Box<AgentRuntime>>),  // Option 允许 .take()
    Offline { reason: String },
}
```

在所有现有的 `RuntimeBuild::Ready(rt)` match arm 里，把 `rt` 改成
`rt_opt.as_mut().unwrap()` 或 `let rt = rt_opt.as_mut().unwrap();`
（非 turn path 不需要 take，turn path 用 `.take().unwrap()`）。

Turn path 统一模式：

```rust
UserAction::SendMessage(text) => {
    if let RuntimeBuild::Ready(rt_opt) = &mut state {
        let pre_turn_len = rt_opt.as_ref().unwrap().transcript().len();
        // take ownership
        let rt = rt_opt.take().unwrap();
        let rt_shared = Arc::new(tokio::sync::Mutex::new(rt));
        let rt_clone = rt_shared.clone();

        cancel_flag.store(false, Ordering::SeqCst);
        let cancel_clone = cancel_flag.clone();

        let mut handle = tokio::task::spawn(async move {
            let mut g = rt_clone.lock().await;
            g.run(text).await.map(|_| ())
        });

        let aborted = tokio::select! {
            res = &mut handle => {
                if let Err(e) = res.and_then(|r| r.map_err(|e| /* wrap */ e)) {
                    let _ = event_tx.send(UiEvent::Error { message: e.to_string() });
                }
                false
            },
            _ = wait_for_cancel(cancel_clone) => {
                handle.abort();
                let _ = handle.await;
                let _ = event_tx.send(UiEvent::Interrupted);
                true
            }
        };

        // recover runtime
        let mut recovered = Arc::try_unwrap(rt_shared).ok()
            .expect("single owner after task end")
            .into_inner();

        if aborted {
            // truncate to pre-turn to avoid orphan tool_calls
            recovered.truncate_transcript(pre_turn_len);
        }

        // put back
        *rt_opt = Some(recovered);
        let _ = event_tx.send(UiEvent::TurnFinished);
        cancel_flag.store(false, Ordering::SeqCst);
    } else if let RuntimeBuild::Offline { reason } = &state {
        let _ = event_tx.send(UiEvent::Error { message: reason.clone() });
        let _ = event_tx.send(UiEvent::TurnFinished);
    }
}
```

## Scope

### 1. `src/tui/runtime_builder.rs`

Change `RuntimeBuild::Ready(Box<AgentRuntime>)` → `RuntimeBuild::Ready(Option<Box<AgentRuntime>>)`.

### 2. `src/runtime.rs`

Add a one-liner helper method to `AgentRuntime` (in `src/runtime.rs`, which we
*are* allowed to minimally extend with a builder/helper method):

```rust
/// Truncate transcript to `len` messages (used by TUI abort to restore
/// pre-turn state and avoid orphan tool_call entries).
pub fn truncate_transcript(&mut self, len: usize) {
    self.messages.truncate(len);
}
```

This is a 3-line addition. The goal doc says "只允许加一个 builder 方法" — this fits.

### 3. `src/tui/backend.rs`

Apply the turn-abort pattern to all 4 turn paths:
- `UserAction::SendMessage` (line ~203)
- `UserAction::ConfirmPlan` (line ~234)
- `UserAction::SetGoal` (line ~291)
- `UserAction::RunSkillPrompt` (line ~332)

For each: replace the old `tokio::select! { r = rt.run(...), _ = wait_for_cancel }` with the `Option::take → spawn → select! → abort → recover → truncate_if_aborted → put_back` pattern above.

Update the event sink and permission hook wiring at line ~186 to use `rt_opt.as_mut().unwrap()` instead of `rt`.

All other `match &mut state { RuntimeBuild::Ready(rt) => ... }` arms that do NOT spawn — e.g., `Compact`, `SetPlanningMode`, `RejectPlan`, `ClearGoal` — simply change to `RuntimeBuild::Ready(rt_opt) => { let rt = rt_opt.as_mut().unwrap(); ... }`.

### 4. Tests

In `src/tui/backend.rs` `#[cfg(test)]` block, add:
- `abort_cancels_inflight_turn`: create a mock runtime that sleeps for 5s in
  `run()`, set cancel_flag=true immediately, verify the select! branch returns
  quickly (< 500ms) and `UiEvent::Interrupted` was sent.
  Use `tokio::time::timeout` to bound the test.

## Acceptance

- `cargo test --workspace` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- All 4 turn paths use the spawn+abort pattern
- Pressing Esc/Ctrl+C while a turn is in flight emits `UiEvent::Interrupted`
  in < 200ms (reqwest connection drops immediately)
- Next turn after abort sends successfully (no orphan tool_call in transcript)

## Notes for the agent

- `Arc::try_unwrap(rt_shared).ok().expect("single owner")` will panic if
  the spawned task is still holding the lock — this CAN'T happen because:
  (a) on normal completion, the task completed before we reach `try_unwrap`
  (b) on abort, `handle.await` drained the task before `try_unwrap`
  So the panic branch is unreachable in practice.
- `AgentRuntime` is `Send` (all its fields are `Arc<dyn ... + Send + Sync>`),
  so `tokio::task::spawn` works.
- `truncate_transcript` is safe to call because after abort the task is fully
  drained — no other references to the runtime exist.
- The 4 turn paths are the only places that call `rt.run(...)` /
  `rt.run_goal_loop(...)`. The `Compact`, `SetPlanningMode`, `RejectPlan`,
  `ClearGoal`, `RunShell` paths do NOT need spawn+abort — they are either
  synchronous or short async ops.
- Do NOT change `wait_for_cancel` — keep it as the select! condition.
- `UiEvent::Interrupted` is already defined in `src/tui/events.rs` and
  handled in `src/tui/app.rs` (added in partial commit c17b732).
  Do NOT re-add it. Just send it from backend.
- **DO NOT** change `src/agent.rs` beyond the `truncate_transcript` addition
  to `src/runtime.rs`.

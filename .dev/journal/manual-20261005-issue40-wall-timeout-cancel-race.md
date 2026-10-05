# Journal — issue #40: 聚合超时分支把「自己触发的取消」误标为 Cancelled（wall-clock 测试 flaky）

**Date**: 2026-10-05
**Goal**: 修复 `cargo test --workspace` 失败门——`-p recursive-agent --test
issue40-parallel-agent-budget`。该测试集在满负载下曾单次失败（见
`.dev/journal/manual-20261002-session-id-collision.md` 的 Notes：`…wall_clock_exceeded`
failed once under full-suite load）。

## Root cause

`AgentTool::execute_parallel` 的聚合超时/取消分支（`src/tools/agent.rs`）顺序是：

1. `child_token.cancel()`
2. 读「结果登记表」（`rescued`）抢救已完成 worker 的真实结果
3. 未完成 worker → abort + 占位

worker 侧 `RunCore::complete_with_budget`（`src/run_core.rs:1016-1045`）把 provider
调用与 `token.cancelled()` / wall-budget sleep 一起 `select!`。于是：

- `parallel_workers_token_present_wall_deadline_labels_wall_clock_exceeded` 里
  父级聚合 deadline 与 worker 自己的 wall budget 都是 5s，两者几乎同时到期；
  父级先醒 → 调 `child_token.cancel()`；
- worker 被唤醒后 `select!` 命中 `token.cancelled()` → `Err(Error::Cancelled)`
  → `FinishReason::Cancelled` → `run_worker` 返回 `"[worker 'x' finished: Cancelled]"`;
- worker 在父级执行到第 2 步（`rescued.lock()`，与第 1 步之间**没有 await**，但
  测试用的是 `flavor = "multi_thread"`，worker 跑在另一个线程上）之前把这条结果写进登记表；
- 父级读到它 → 聚合结果里出现 `Cancelled` → 断言
  `!result.contains("Cancelled")` 失败。

即：**用超时自己的取消动作去判定已完成结果**，取消的副作用污染了抢救数据。
单核（current_thread）运行时该竞态不可达，所以只在满负载 / 多线程下偶发。

## Fix

先对登记表做快照，**再**传播取消（快照是 id-keyed 的整表 clone，不是按 index
映射的 `Vec`；`manifest.keys()` 与 `handles` 同序，`debug_assert_eq!` 兜底）：

```rust
let ids: Vec<String> = manifest.keys().cloned().collect();
debug_assert_eq!(ids.len(), handles.len());
let rescued_before_cancel = {
    let ledger = rescued.lock().unwrap_or_else(|e| e.into_inner());
    ledger.clone()                       // BTreeMap<String, Result<String, String>>
};
if let Some(token) = &child_token { token.cancel(); }
for (idx, handle) in handles.into_iter().enumerate() {
    let id = ids.get(idx).cloned().unwrap_or_else(|| "(unknown)".into());
    match rescued_before_cancel.get(&id) {   // 按 id 查，不依赖位置对齐
        Some(Ok(text)) => results.push((id, text.clone())),
        Some(Err(e)) => results.push((id, format!("ERROR: {e}"))),
        None => { /* abort + 占位 */ }
    }
}
```

快照之后才 `cancel()`，因此登记表里不可能出现「由本次 cancel 造成」的
`Cancelled` 条目——wall-clock 超时只能拿到 `WallClockExceeded`（worker 自己
budget 先到）或占位 `WallClockExceeded`（父级 deadline 先到）。取消传播、
abort 语义与占位行为保持不变。

## Files touched

- `src/tools/agent.rs` — `execute_parallel` 聚合分支：ledger 快照早于
  `child_token.cancel()`（去掉了原来的 `drop(ledger)`，改为块作用域）。

## Tests

未新增测试：该竞态是「父级 cancel 与 worker 落表」在同一任务内无 await 的
极窄窗口，无法在不加插桩的前提下确定性复现（current_thread 下不可达，
multi_thread 下需要靠调度）。既有 7 个 issue40 集成测试覆盖该分支的所有可见
行为（WallClockExceeded / Cancelled / 抢救已完成结果 / 配对占位）。

## Verification

- `cargo fmt --all` — 无改动。
- `cargo clippy --all-targets --all-features -- -D warnings` — clean（exit 0，7m）。
- `cargo test --workspace` — exit 0，59 个 test target 全 ok，0 failed
  （含 40 个 CPU hog 的满负载重跑）。
- `cargo test -p recursive-agent --test issue40-parallel-agent-budget` — 5/5 连续通过。

## Notes

- 同一分支里 `execute_single` / `execute_sequential` 无登记表，标签由「哪个分支
  先醒」直接决定，不存在该竞态，未改动。
- 工作区另有一批未提交的 goal-124（Langfuse/OTLP observability）改动，与本修复无关。

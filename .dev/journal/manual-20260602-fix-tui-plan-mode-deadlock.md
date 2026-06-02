# Manual edit: fix-tui-plan-mode-deadlock

**Date**: 2026-06-02
**Goal**: 修复 TUI 中 plan 模式的两个严重 bug：
1. `exit_plan_mode` 工具阻塞时 backend worker 无法接收 ConfirmPlan/RejectPlan 导致永久死锁
2. 状态栏没有显示 plan 等待审批 / plan-first 模式的指示器

**Root Cause**:
backend 的 `worker_loop` 在处理 `SendMessage` / `ConfirmPlan` / `SetGoal` 时，
会把运行时移入 tokio 任务，然后在 `tokio::select! { handle | cancel_flag }` 中阻塞等待。
这期间 `action_rx` 从不被轮询，所以用户在 PlanReview modal 按 y/n 发出的
`ConfirmPlan` / `RejectPlan` UserAction 永远无法被接收，`PlanApprovalGate` 的
notify 从不触发，`exit_plan_mode` 工具永久挂起。

**Fix**:
1. 提取 `run_turn_select_loop` 辅助函数，在 task 运行期间同时轮询 `action_rx`：
   - `ConfirmPlan` → `gate.approve()`
   - `RejectPlan(reason)` → `gate.reject(&reason)`
   - `Interrupt` → 设置 cancel_flag
   - 其他 action → 丢弃（运行时在 task 内部，无法服务）
2. `SendMessage` / `ConfirmPlan` / `SetGoal` handler 均改用该辅助函数
3. `App` 添加 `plan_awaiting_approval: bool` 字段，由 PlanProposed/Confirmed/Rejected/Interrupted 事件维护
4. `status.rs` 状态栏在 `plan_awaiting_approval=true` 时显示醒目的 `[plan: y/n]` 黄底指示器，
   在 `planning_mode_on=true` 时显示 `plan-first` 提示

**Files touched**:
- `src/tui/backend.rs`
- `src/tui/app.rs`
- `src/tui/ui/status.rs`

**Tests added**:
- `status_bar_shows_plan_awaiting_indicator`
- `status_bar_shows_plan_first_mode`

**Notes**:
在 run_turn_select_loop 中，非 plan/interrupt action 会被丢弃，因为运行时在 task 内部无法访问。
正常使用时只有 plan 审批和中断操作会在 turn 运行期间到来，丢弃其他操作不影响可用性。
后续可以添加 deferred action queue 来更完整地处理这种情况（Goal 留给未来）。

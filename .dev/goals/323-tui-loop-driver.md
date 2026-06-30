# Goal 323 — TUI event-driven loop driver (P1)

**Roadmap**: 详见 `.dev/proposals/tui-loop-driver.md`（本 goal 是该 proposal 的 P1 切片）

**Design principle check**:
- Implemented as: TUI backend 层新增 `LoopArbiter` + `UserAction::StartLoop/StopLoop/LoopTrigger`；kernel 层只给 `BackgroundJobManager` 加 `Notify` + 注入 API。
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
- ❌ Does NOT call `run_event_loop`（方案 B: backend 自驱动，绕开它）

## Why

Recursive 已有 `schedule_wakeup` + `run_loop`（定时）和 `run_event_loop`（后台任务完成触发），但事件驱动 loop 没有产品 surface：CLI `recursive loop` 调的是 `runtime.run_loop` 不是 `run_event_loop`，TUI 完全没接。loop 真正可用的场合是 TUI（常驻、可插话、可中断）。本 goal 在 TUI backend 实现事件驱动 loop，让「等 shell 完成 + 定时 + 用户插话」三种触发同构，统一在 backend `select!` 里仲裁。

完整设计见 `.dev/proposals/tui-loop-driver.md`。**本 goal 只做 P1**。

## Scope (do exactly this, no more)

### P1 范围（三源: BgComplete + ScheduleWakeup + 用户插话）

### 1. kernel: `src/tools/run_background.rs`

- 给 `BackgroundJobManager` 加 `completed_notify: Arc<tokio::sync::Notify>`，任务进入终态（Completed/Failed/Stopped/Timeout）时 `notify_waiters()`。
- 暴露 `pub fn completed_notify(&self) -> Arc<Notify>`。
- 加注入 API：让外部传入共享 `Arc<Mutex<BackgroundJobManager>>`，使 `RunBackground`/`CheckBackground` 工具与 backend 看到同一份任务。形式可选：`ToolRegistry::with_background_manager(...)` 或 `build_standard_tools_with_roots` 增加可选参数。选最干净的，与现有 `registry.rs:579` 内部自建那份兼容。
- **不改动 `run_event_loop`**（保留，本 goal 不调用它）。

### 2. TUI runtime: `crates/recursive-tui/src/runtime_builder.rs`

- `build_runtime` / `build_runtime_for_tui` 返回 `WakeupSlot` + `Arc<Mutex<BackgroundJobManager>>` 给 backend（新增返回结构体 `TuiRuntime` 或扩展现有返回）。
- 注册 `ScheduleWakeup::new(wakeup_slot.clone())` 到工具集。
- 用注入 API 让 `RunBackground`/`CheckBackground` 共享同一 `bg_manager`。

### 3. TUI events: `crates/recursive-tui/src/events.rs`

新增 `UserAction` 变体：
```rust
StartLoop { goal: String, max_turns: u32 },  // 0 = unlimited
StopLoop,
LoopTrigger { source: String, prompt: String },
```

新增 `UiEvent` 变体：
```rust
LoopStarted { goal: String },
LoopStopped,
LoopTurnScheduled { source: String, delay_secs: Option<u64> },
LoopIdle,
```

（`LoopSourceToggle` / `LoopSource` 枚举属 P2，**本 goal 不加**。）

### 4. TUI backend: `crates/recursive-tui/src/backend.rs`

- 主循环顶部加 loop 仲裁：drain `queued_messages` 后，若 `loop_state.active`，调 `loop_arbiter` 选下一 turn prompt；否则现状。
- 新增 `LoopState { active, turns_run, max_turns, sources }`（P1 sources 固定 `BgComplete + ScheduleWakeup`，可硬编码，不必加 `HashSet`）。
- `loop_arbiter` 用 `tokio::select! { biased; ... }`，优先级（已确认）：
  1. 用户 `StopLoop`/`Interrupt`/`Shutdown` → `ArbiterDecision::Stop`
  2. 用户 `SendMessage` → 入 `queued_messages`，返回 `Idle`（下轮 drain）
  3. `bg_notify.notified()` → `bg_manager.take_completed()`，拼成 `format!("Background job '{}' completed:\n{}", id, out)`（与 `run_event_loop:937` 同格式）
  4. `wakeup_slot.take()` + `sleep(delay)` → 用 `req.prompt`
  - 虚唤醒（`take_completed` 返回 None）→ `Idle`
  - wakeup 分支被动等待：slot 为空时永远不返回，靠 bg/用户唤醒
- 选定 prompt 后走现有 `spawn(g.run(prompt))` → `run_turn_select_loop` → recover runtime 路径，**复用不重写**。
- `StartLoop` 分支：若 `runtime.has_goal()` → `UiEvent::Error`（互斥）；否则建 `SessionWriter`（**关键坑**：现有 writer 在首个 `SendMessage` 才建，`backend.rs:469`；loop turn 也要落盘）→ 发 `LoopStarted` → 进 arbiter。
- `StopLoop`：设 `loop_state.active = false`，发 `LoopStopped`（当前 turn 跑完才停，不强杀）。
- `max_turns` 达到 → 自动 `LoopStopped`。
- `SetGoal` 时若 `loop_state.is_some()` → `UiEvent::Error`（反向互斥）。

### 5. TUI commands: `crates/recursive-tui/src/commands.rs` / `command_menu.rs`

```
/loop start <goal>        → StartLoop { goal, max_turns: 0 }
/loop start <goal> max N  → StartLoop { goal, max_turns: N }
/loop stop                → StopLoop
/loop trigger <text>      → LoopTrigger { source: "manual", prompt: text }
```
（`/loop on|off <source>` 属 P2，不加。）

### 6. TUI status bar: `crates/recursive-tui/src/ui/status.rs`

新增 loop 状态指示：`loop: on [bg+wait] turn N` / `loop: idle` / `loop: off`。

### 7. Tests

**kernel** (`src/tools/run_background.rs`)：
- `completed_notify` 任务完成时唤醒等待者（`tokio::time::timeout` 包，防 spurious 挂死）。
- 多等待者 `notify_waiters` 全唤醒（文档化）。
- 注入 API 后 `RunBackground` 工具与外部 `take_completed` 看到同一份任务。

**TUI backend** (`crates/recursive-tui/src/backend.rs` tests，复用 `Backend::spawn` + `MockProvider` harness)：
- `StartLoop` 后 agent 调 `schedule_wakeup(1s)` → 1s 后第二个 turn 用 `req.prompt`。
- agent `run_in_background: true` shell → `BgComplete` 唤醒 → 下一 turn prompt 含 "Background job '...' completed"。
- arbiter 等待期间用户 `SendMessage` → 入队，下轮 drain（插话优先于 wakeup）。
- `StopLoop` 等待期间到达 → `LoopStopped`，不再跑 turn。
- `max_turns` 达到 → 自动 `LoopStopped`。
- `StartLoop` 与 `SetGoal` 互斥双向。
- `LoopTrigger{source:"manual",prompt}` 立即触发一个 turn。
- bg 完成 + wakeup 同时到来 → 选 bg 完成（biased 顺序）。

## Acceptance

- `cargo fmt --all -- --check` clean
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo test --workspace` green
- `.dev/scripts/tui-test-presence.sh` exit 0
- `.dev/scripts/tui-mutants.sh` exit 0（`loop_arbiter` 决策分支必须被测试钉住）
- 手动：TUI 里 `/loop start watch the build` + agent 起一个 `run_in_background: true` shell → shell 完成后 agent 自动收到 "Background job '...' completed" 并继续 turn

## Notes for the agent

- **先读** `.dev/proposals/tui-loop-driver.md` —— 完整设计与决策理由都在那里。本 goal 是该 proposal 的 P1 切片，严格按 §4 已确认决定执行（bg 完成 > wakeup 优先级；与 SetGoal 互斥；不调 run_event_loop）。
- **开工前跑** `gitnexus_impact({target: "BackgroundJobManager", direction: "upstream"})` 和 `gitnexus_impact({target: "run_event_loop"})`，确认 blast radius。HIGH/CRITICAL 风险先停下报告。
- **复用 `run_turn_select_loop`**，不要重写 turn-间控制逻辑。
- **Session writer 坑**：`StartLoop` 分支必须建 `SessionWriter`（参考 `backend.rs:469` 的 `SendMessage` 分支），否则 loop turn 不落盘。
- **不要加 P2 内容**：`FileWatch`/`Webhook`/`Signal`/`Cron`/`LoopSourceToggle`/`LoopSource` 枚举一律不加。
- **`run_event_loop` 不删不动**，本 goal 绕开它。
- `schedule_wakeup` 上限 3600s 保持不变。
- 触碰 `crates/recursive-tui/src/` 必须过 TUI gates（presence + mutants），flow 会自动跑，但本地先过。
- **DO NOT modify** `src/agent.rs`、`src/runtime.rs::run_event_loop` 的语义、CLI `recursive loop` 子命令、HTTP handlers。本 goal 只动 TUI + `run_background.rs`。

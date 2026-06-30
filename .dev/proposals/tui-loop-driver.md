# Proposal — TUI Loop Driver (事件驱动 loop)

**状态**: 草案, 待开 goal
**作者**: manual design session (2026-06-30)
**关联**: `src/runtime.rs::run_event_loop`, `src/tools/schedule_wakeup.rs`,
`src/tools/run_background.rs::BackgroundJobManager`,
`crates/recursive-tui/src/backend.rs`

## 1. 动机

Recursive 已有三种多轮驱动模式 (`run` / `run_goal_loop` / `run_loop` /
`run_event_loop`), 但事件驱动 loop 在产品层没有可用入口:

- `schedule_wakeup` 工具 + `run_loop` 已实现定时唤醒, 但 CLI `recursive loop`
  子命令无人盯, 语义弱。
- `run_event_loop` 实现了「后台任务完成触发」, 但**没有产品 surface 接入**
  (CLI `run_loop` 调的是 `runtime.run_loop`, 不是 `run_event_loop`)。
- 没有文件监听 / webhook / 信号等外部事件触发入口。

**判断**: loop 真正可用的场合是 TUI。TUI backend 是常驻 event loop, 已有
`UserAction` / `UiEvent` 双向通道、`run_turn_select_loop` (turn 间响应
cancel/plan/插话)、`queued_messages` (type-ahead)、`Interrupt`/`cancel_notify`,
以及已接通的 `run_goal_loop`。CLI 一次性调用与 loop 语义矛盾; HTTP 渠道
`POST /sessions/:id/messages` 已能注入消息触发 turn, 再加 loop 是功能重叠。

**结论**: 把事件驱动 loop 集中到 TUI backend, **方案 B — backend 层自驱动,
不调用 `run_event_loop`**, 让「等 shell 完成 / 定时 / 外部事件 / 用户插话」
成为同构触发源, 统一在 backend 的 `select!` 里仲裁。

## 2. 现状回顾

backend 主循环 (`backend.rs:367`) 结构:

- `loop {}` 顶部 `queued_messages.pop_front()` 先 drain type-ahead (FIFO)
- 每个 `SendMessage` / `SetGoal` / `RunSkillPrompt`: take runtime →
  `tokio::task::spawn(g.run/g.run_goal_loop)` → `run_turn_select_loop` 边跑
  边 select action → `Arc::try_unwrap` recover runtime → `TurnFinished`
- runtime 在 turn 之间**始终空闲可用**, 不被任何循环长期持有

=> loop 模式 = 主循环顶部「该跑下一 turn 时从多触发源挑一个 prompt」的扩展。
每个 turn 仍走现有 spawn → select → recover 路径。

## 3. 总体架构

```
┌──────────────────────── TUI backend (常驻) ────────────────────────┐
│  主循环 loop {}                                                      │
│   ├─ 1. drain queued_messages (type-ahead, FIFO)                    │
│   ├─ 2. if loop_mode.active → LoopArbiter 仲裁下一 turn             │
│   │      else → action_rx.recv() (现状)                             │
│   ├─ 3. 选定 prompt → spawn g.run(prompt)                           │
│   ├─ 4. run_turn_select_loop (cancel/plan/插队消息, 现状不变)         │
│   └─ 5. recover runtime → 回到 1                                    │
│                                                                     │
│  LoopArbiter (tokio::select! biased):                               │
│   ├─ StopLoop / Interrupt (action_rx)  ← 用户随时可停                │
│   ├─ SendMessage (action_rx)           ← 用户插话, 入队下轮 drain     │
│   ├─ bg_manager.completed_notify       ← 后台 shell 完成 (优先于 wakeup)│
│   ├─ trigger_rx.recv()                 ← 外部事件 (P2+: file/webhook/signal)│
│   └─ wait_wakeup(wakeup_slot) + sleep  ← 定时唤醒 (agent 自调度)      │
│                                                                     │
│  独立 tokio tasks (P2+ 按需): file watcher / webhook listener /     │
│  signal handler → trigger_tx                                        │
└─────────────────────────────────────────────────────────────────────┘
          ↑ 共享                  ↑ 共享
┌───────── runtime_builder ─────────┐
│  WakeupSlot (新, 注册 ScheduleWakeup)│
│  Arc<Mutex<BackgroundJobManager>> (暴露给 backend)
└───────────────────────────────────┘
```

## 4. 已确认的设计决定

| 决策点 | 选择 |
|---|---|
| 仲裁优先级 | bg 完成 > 定时 wakeup (数据已就绪优先) |
| P1 范围 | 仅 BgComplete + ScheduleWakeup + 用户插话 |
| 与 `SetGoal` 的关系 | **互斥**: 已有 goal 时 `StartLoop` 报 `UiEvent::Error` |
| 载体 | 仅 TUI, 不动 CLI / HTTP |
| kernel 改动 | 最小: `BackgroundJobManager` 加 Notify; **不调用 `run_event_loop`** |

## 5. 新增类型

### `events.rs::UserAction`

```rust
// ── Loop mode (event-driven) ───────────────────────────────────────
StartLoop {
    /// 初始 goal, 也是第一个 turn 的 prompt。
    goal: String,
    /// 自主 turn 上限, 0 = 不限。
    max_turns: u32,
},
/// 退出 loop 模式 (当前 turn 跑完后停止, 不强杀)。
StopLoop,
/// 手动注入一次触发 (测试 / 手工推进)。
LoopTrigger {
    source: String,
    prompt: String,
},
/// 启用 / 禁用某类触发源 (P2+ 用)。
LoopSourceToggle {
    source: LoopSource,
    enabled: bool,
    config: serde_json::Value,
},
```

`LoopSource` 枚举: `BgComplete` / `ScheduleWakeup` / `FileWatch` / `Webhook` /
`Signal` / `Cron`。P1 默认开启 `BgComplete + ScheduleWakeup`。

### `events.rs::UiEvent`

```rust
LoopStarted { goal: String },
LoopStopped,
LoopTurnScheduled {
    source: String,      // "wakeup" | "bg-complete" | "manual" | "file-watch" | ...
    delay_secs: Option<u64>,
},
LoopIdle,                // 等待触发中 (spinner 用)
LoopSourceChanged { source: String, enabled: bool },
```

## 6. Backend 状态新增

```rust
struct LoopState {
    active: bool,
    turns_run: u32,
    max_turns: u32,                  // 0 = unlimited
    sources: HashSet<LoopSource>,    // 默认 {BgComplete, ScheduleWakeup}
}

let mut loop_state: Option<LoopState> = None;

// 来自 runtime_builder, 整个 session 共享
let wakeup_slot: WakeupSlot = shared.wakeup_slot;
let bg_manager: Arc<Mutex<BackgroundJobManager>> = shared.bg_manager;
let bg_notify = bg_manager.lock().await.completed_notify();

// 外部触发源汇入 (P2+ 按需 spawn task 写入)
let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel::<LoopTrigger>();
```

互斥: `loop_state.is_some()` 与现有 goal state 不能共存。
`StartLoop` 时若 `runtime.has_goal()` → `UiEvent::Error`; 反之 `SetGoal`
时若 `loop_state.is_some()` → `UiEvent::Error`。

## 7. 核心组件: LoopArbiter

主循环顶部:

```rust
loop {
    // 1. type-ahead 优先
    if let Some(text) = queued_messages.pop_front() {
        run_one_turn(text, ...).await;
        continue;
    }

    // 2. loop 模式: 仲裁下一 turn
    if let Some(ls) = loop_state.as_mut().filter(|s| s.active) {
        let next = loop_arbiter(
            ls, &mut action_rx, &wakeup_slot, &bg_manager, &bg_notify,
            &mut trigger_rx, &cancel_notify, &event_tx,
        ).await;

        match next {
            ArbiterDecision::Run { prompt, source, delay } => {
                let _ = event_tx.send(UiEvent::LoopTurnScheduled { source, delay_secs: delay });
                run_one_turn(prompt, ...).await;
                ls.turns_run += 1;
                if ls.max_turns > 0 && ls.turns_run >= ls.max_turns {
                    let _ = event_tx.send(UiEvent::LoopStopped);
                    loop_state = None;
                }
            }
            ArbiterDecision::Stop => {
                let _ = event_tx.send(UiEvent::LoopStopped);
                loop_state = None;
            }
            ArbiterDecision::Idle => {
                let _ = event_tx.send(UiEvent::LoopIdle);
            }
        }
        continue;
    }

    // 3. 非 loop 模式: 现状
    let action = action_rx.recv()...;
    ...
}
```

`loop_arbiter` (伪代码, 优先级: 用户控制 > bg 完成 > 外部触发 > 定时 wakeup):

```rust
async fn loop_arbiter(...) -> ArbiterDecision {
    // 优先级 1: 用户控制动作 (非阻塞探一次)
    while let Ok(Some(action)) = nonblocking_recv(&mut action_rx) {
        match action {
            UserAction::StopLoop | UserAction::Interrupt | UserAction::Shutdown =>
                return ArbiterDecision::Stop,
            UserAction::SendMessage(text) => queued.push_back(text), // 下轮 drain
            UserAction::LoopTrigger{ source, prompt } =>
                return ArbiterDecision::Run { prompt, source, delay: None },
            _ => {}
        }
    }

    // 优先级 2..N: 阻塞 select 等第一个到来的触发源
    // biased 顺序 = 优先级
    let prompt = tokio::select! {
        biased;
        // 用户等待期间停止
        action = action_rx.recv() => match action {
            Some(UserAction::StopLoop | Interrupt | Shutdown) => return Stop,
            Some(UserAction::SendMessage(t)) => { queued.push_back(t); return Idle; }
            Some(UserAction::LoopTrigger{ source, prompt }) =>
                return Run { prompt, source, delay: None },
            _ => return Idle,
        };
        // 后台任务完成 (优先于 wakeup: 数据已就绪)
        _ = bg_notify.notified() => {
            if let Some((id, out)) = bg_manager.lock().await.take_completed() {
                (format!("Background job '{}' completed:\n{}", id, out),
                 "bg-complete".to_string(), None)
            } else { return Idle; }  // 虚唤醒
        }
        // 外部触发 (P2+)
        trig = trigger_rx.recv() => match trig {
            Some(t) => (format!("[trigger:{}] {}", t.source, t.prompt),
                        t.source, None),
            None => return Idle,
        }
        // 定时唤醒 (agent 自调度, 被动等待)
        req = wait_wakeup(&wakeup_slot) => match req {
            Some(req) => {
                sleep(req.delay).await;
                (req.prompt, "wakeup".to_string(), Some(req.delay.as_secs()))
            }
            None => return Idle,  // 没人 schedule → 永远不醒, Idle
        }
    };

    ArbiterDecision::Run { prompt: prompt.0, source: prompt.1, delay: prompt.2 }
}
```

**关键语义**: 定时唤醒是被动等待。agent 没调 `schedule_wakeup` 时 wakeup
分支永远不返回, arbiter 一直 select 在「bg 完成 / 外部触发 / 用户动作」上。
这正是「等 shell 完成」想要的: agent 跑完 turn 把 shell 丢后台, loop 不退出,
等 `bg_notify` 唤醒。

## 8. runtime_builder 改动

`runtime_builder.rs` 当前 `build_standard_tools_with_roots(...)` 内部自建
`BackgroundJobManager` (`tools/registry.rs:579`) 但不暴露。改为:

```rust
pub struct TuiRuntime {
    pub runtime: Option<Box<AgentRuntime>>,
    pub wakeup_slot: WakeupSlot,
    pub bg_manager: Arc<tokio::sync::Mutex<BackgroundJobManager>>,
    pub session_roots: SharedSandboxRoots,
}

pub fn build_runtime_for_tui() -> TuiRuntime {
    let wakeup_slot: WakeupSlot = Arc::new(Mutex::new(None));
    let bg_manager = Arc::new(tokio::sync::Mutex::new(BackgroundJobManager::new()));

    let tools = build_standard_tools_with_roots(...);
    let tools = tools
        .register(Arc::new(ScheduleWakeup::new(wakeup_slot.clone())))
        .with_background_manager(bg_manager.clone());   // 新 API, 见 §9
    ...
    TuiRuntime { runtime, wakeup_slot, bg_manager, session_roots }
}
```

让 `RunBackground` / `CheckBackground` 使用 backend 提供的 `bg_manager`,
而不是内部自建的那份。

## 9. kernel 层改动 (最小, 全在 `src/tools/run_background.rs`)

1. **`BackgroundJobManager` 加完成通知**:

```rust
pub struct BackgroundJobManager {
    // ... 现有字段
    completed_notify: Arc<tokio::sync::Notify>,
}

impl BackgroundJobManager {
    pub fn new() -> Self { ... completed_notify: Arc::new(Notify::new()) ... }

    fn mark_terminal(&mut self, id: &TaskId, ...) {
        // 现有逻辑
        self.completed_notify.notify_waiters();
    }

    /// 让 backend 在 select! 里 await。
    pub fn completed_notify(&self) -> Arc<Notify> {
        self.completed_notify.clone()
    }
}
```

2. **暴露注入 manager 的 builder API**:
`ToolRegistry::with_background_manager(Arc<Mutex<BackgroundJobManager>>)` 或
`build_standard_tools_with_roots` 增加可选参数, 让 TUI / CLI / 测试都能注入
共享 manager。

`run_event_loop` **不动** (保留给测试, 不再被产品 surface 调用)。

## 10. 外部触发源扩展点 (P2+)

全部独立 `tokio::task`, 事件写入 `trigger_tx`, **不进 kernel**:

| 源 | 实现 | 阶段 |
|---|---|---|
| `FileWatch` | `notify` crate watcher → debounce → trigger_tx | P2 |
| `Webhook` | 轻量 HTTP listener (复用 `src/http` 路由能力, 单独端口) → POST body 当 prompt | P3 |
| `Signal` | `tokio::signal::unix(user1/user2)` → 固定 prompt | P3 |
| `Cron` | 外挂 `cron` 调 webhook, 或内部 `interval_at(下次cron点)` | P4, 建议外挂 |

P1 只做 `BgComplete + ScheduleWakeup + 用户插话`, 已覆盖「等 shell 完成 +
定时」。

## 11. 与 `run_goal_loop` 的关系 (互斥)

| | `SetGoal` (现有) | `StartLoop` (新) |
|---|---|---|
| 驱动 | 条件判定 (内部反复跑直到自判条件满足) | 事件触发 (backend 仲裁) |
| 终止 | 条件 met 或 max_turns | `StopLoop` 或 max_turns |
| runtime 持有 | 长期持有 (spawn `run_goal_loop` 跑完) | 每 turn 之间释放 |
| 可插话 | 否 (要等整个 goal loop 跑完) | 是 (每 turn 之间 drain 插话) |
| 事件 | `GoalContinuing` / `GoalAchieved` | `LoopTurnScheduled` / `LoopIdle` |

## 12. UI 命令

`crates/recursive-tui/src/commands.rs` / `command_menu.rs` 增加:

```
/loop start <goal>        → StartLoop { goal, max_turns: 0 }
/loop start <goal> max N  → StartLoop { goal, max_turns: N }
/loop stop                → StopLoop
/loop trigger <text>      → LoopTrigger { source: "manual", prompt: text }
/loop on <source> [cfg]   → LoopSourceToggle { enabled: true, ... }   (P2+)
/loop off <source>        → LoopSourceToggle { enabled: false, ... }
/loop status              → 查询 loop 状态 (只读)
```

状态栏 (`ui/status.rs`) 新增: `loop: on [bg+wait] turn 3` /
`loop: idle (waiting bg)` / `loop: off`。

## 13. 测试策略

按项目规矩: `#[cfg(test)] mod tests` 同文件 + `cargo test` + `cargo clippy
--all-targets --all-features -- -D warnings` + `cargo fmt --all` + TUI
gates (`tui-test-presence.sh`, `tui-mutants.sh`)。

### kernel 层 (`run_background.rs`)

- `completed_notify` 在任务完成时唤醒等待者。
- 虚唤醒 (无任务完成) 不返回 spurious 信号 (用 `tokio::time::timeout` 包)。
- 多等待者同时 await, `notify_waiters` 全唤醒 (文档化)。
- `with_background_manager` 注入后, `RunBackground` 工具与外部
  `take_completed` 看到同一份任务。

### TUI backend (`backend.rs`, 复用 `Backend::spawn` harness)

- `StartLoop` 后 agent 调 `schedule_wakeup(1s)` → 1s 后收到第二个 turn
  (用 `MockProvider` 脚本)。
- agent 跑 `run_in_background: true` shell → `BgComplete` 唤醒 → 下一 turn
  prompt 含 "Background job '...' completed"。
- 用户在 arbiter 等待期间 `SendMessage` → 入队, 下一 turn 跑用户消息
  (插话优先于 wakeup)。
- `StopLoop` 在等待期间到达 → `LoopStopped`, 不再跑 turn。
- `max_turns` 达到后自动 `LoopStopped`。
- `StartLoop` 与 `SetGoal` 互斥: 已有 goal 时 `StartLoop` → `Error`;
  反之 `SetGoal` 时 `loop_state.is_some()` → `Error`。
- `LoopTrigger{source:"manual",prompt}` 立即触发一个 turn。
- bg 完成 + wakeup 同时到来 → 选 bg 完成 (biased 顺序)。

### TUI mutants gate

`loop_arbiter` 决策逻辑会被 `tui-mutants.sh` 攻击。确保优先级、
Idle/Run/Stop 三分支、互斥检查都被测试钉住, 否则 mutants gate 回滚。

## 14. 分阶段实施

| 阶段 | 内容 | 价值 |
|---|---|---|
| **P1** | kernel: `BackgroundJobManager` 加 Notify + 注入 API; TUI: `build_runtime` 暴露 slot+manager; `StartLoop/StopLoop/LoopTrigger`; arbiter 接 `BgComplete + ScheduleWakeup + 插话`; UI `/loop` 命令 + 状态栏; session writer 在 `StartLoop` 时也建 | 「等 shell 完成 + 定时」可用 |
| P2 | `FileWatch` 触发源 (`notify` crate, 需 `Cargo.toml` 加依赖 + goal 说明) | 文件变化触发 |
| P3 | `Webhook` + `Signal` 触发源 | 外部系统集成 |
| P4 | `Cron` / 触发源配置持久化 | 严肃调度 |

P1 估算: kernel ~80 行 + TUI backend ~200 行 + 测试 ~150 行, 单批可落地。

## 15. 风险与开放问题

1. **agent 不主动起后台任务时 `BgComplete` 永不触发** → loop 一直 Idle。
   符合事件驱动语义, 但 UI 必须清晰提示「loop idle, waiting for bg job /
   wakeup」, 避免误以为卡死。
2. **wakeup 与 bg 完成同时到来** → biased 顺序决定: bg 完成 > wakeup
   (已确认)。文档化。
3. **`BackgroundJobManager` in-memory, 进程重启丢失** (`tasks.rs:13` 明确)。
   TUI 重启后 loop 状态也丢。P1 接受。
4. **loop 模式的 session 写入**: 现有 `SessionWriter` 在第一个 `SendMessage`
   时才建 (`backend.rs:469`)。`StartLoop` 分支也要建, 否则 loop turn 不落盘。
   — P1 必须处理。
5. **`schedule_wakeup` 上限 3600s**: P1 保持不变。agent 想等更久可自己轮询
   短 wakeup。
6. **loop 与普通对话共享 transcript**, 长 loop 可能触发 compaction。期望行为,
   确认 compaction 在 loop turn 之间能正常触发 (现 `Compact` action 在 turn
   之间可用)。
7. **`run_event_loop` 后续是否删除**: P1 保留 (测试在用)。未来可标
   `#[deprecated]` 引导迁移, 但不强制。

## 16. Invariant 合规

- **Invariant #1** (agent loop 不膨胀): 本方案在 backend 层驱动, 不在
  `agent.rs::Agent::run` 加分支。kernel 只加 `BackgroundJobManager::Notify`
  + 注入 API。✅
- **Invariant #3** (sandbox): 不涉及 fs/shell 路径解析变更。✅
- **Invariant #5** (无 unwrap): arbiter 用 `match` / `?`, 无 `unwrap`。✅
- **Invariant #8** (tool-call ↔ tool-result pairing): loop turn 之间
  recover runtime, transcript 连续追加, 不做 splice/trim, 配对不变。✅

## 17. 下一步

1. 起 goal 文件 `.dev/goals/NN-tui-loop-driver.md` (P1 范围)。
2. 开工前跑 `gitnexus_impact({target: "BackgroundJobManager", direction:
   "upstream"})` + `gitnexus_impact({target: "run_event_loop"})` 确认 blast
   radius。
3. 按 P1 实施, 走 Flowcast self-improve flow 或 manual 编辑 (后者需手动跑
   `cargo test --workspace` + `cargo clippy --all-targets --all-features
   -- -D warnings` + `cargo fmt --all` + TUI gates)。

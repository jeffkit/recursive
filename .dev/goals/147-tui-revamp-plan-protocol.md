# Goal 147 — TUI Revamp Step 5: Plan Mode 协议化 + 双击 Esc/Ctrl+C 中断

**Roadmap**: Phase 11 — TUI 大改造对齐 fake-cc 风格 (part 5/5)

**Design principle check**:
- 仅改 `crates/recursive-tui/`，不动核心库 trait / 事件定义
- 利用已有的 `AgentEvent::PlanProposed` / `PlanConfirmed` / `PlanRejected`
  和 `AgentRuntime::confirm_plan` / `reject_plan`
- 双击逻辑纯 UI 状态，与 `crossterm` 已有 modifier 信息配合即可

## Why

Goal 143 完成时保留了一个技术债：Plan Mode 用字符串前缀 `"plan:"` /
`"## plan"` 识别（旧 `main.rs:367-372`）。这是脆弱的 hack —— LLM 输出
任何带 "plan:" 的散文都会触发，server 端也无契约可言。

实际上 Recursive 已经有结构化协议：`AgentEvent::PlanProposed { plan_text,
tool_calls }` 在 `src/event.rs` / `src/agent.rs` 中定义并被 kernel 在
planning_mode 下发出。本步把 TUI 接到这个协议，删除前缀 hack。

同时，长 turn 跑起来后用户没有可靠的中断手段：当前 `Esc` 直接退出
TUI（旧 `main.rs:348-350`），`Ctrl+C` 也是。这违反 fake-cc 的双击模式：
**第一次按 = 中断当前操作，2 秒内再按 = 真退出**。本步把这个细节做对。

## Scope (do exactly this, no more)

### 1. PlanReview Modal

把 Goal 143 保留的 `AppScreen::PlanReview` 旧行为重写为一个 Modal：

```rust
// 在 ui/modal.rs::Modal enum 增加
Modal::PlanReview {
    plan_text: String,
    tool_calls: Vec<recursive_agent::llm::ToolCall>,
    // 用户编辑后的版本（如果按 e 编辑）
    edited_text: Option<String>,
}
```

渲染（在 `ui/modal.rs` 或新 `ui/plan_review.rs`）：

```
 ╭─ Plan Proposal ─────────────────────────────────╮
 │                                                  │
 │ <plan_text 多行渲染>                             │
 │                                                  │
 │ Pending tools (3):                               │
 │   • read_file(path="src/agent.rs")               │
 │   • apply_patch(...)                             │
 │   • run_shell(cmd="cargo test")                  │
 │                                                  │
 ╰──────────────────────────────────────────────────╯
   [y/Enter] Approve   [n/Esc] Reject   [e] Edit
```

modal 出现时：

- 优先级最高，遮蔽 chat 键位
- 不允许用户在 transcript 中输入新消息（输入框冻结）

### 2. 接到 PlanProposed 事件

在 `backend.rs::TuiEventSink::emit`：

```rust
AgentEvent::PlanProposed { plan_text, tool_calls } => {
    self.tx.send(UiEvent::PlanProposed { plan_text, tool_calls })?;
}
AgentEvent::PlanConfirmed => {
    self.tx.send(UiEvent::PlanConfirmed)?;
}
AgentEvent::PlanRejected { reason } => {
    self.tx.send(UiEvent::PlanRejected { reason })?;
}
```

`UiEvent` 增加这三个变体。`AppState::apply_event` 中：

- `PlanProposed` → `modals.push(Modal::PlanReview { ... })`，并把 transcript
  push 一个 `System` 块说 "Plan proposed, awaiting approval…"
- `PlanConfirmed` → 关闭可能存在的 PlanReview modal，push System 块 "Plan
  approved"
- `PlanRejected { reason }` → 同上，push "Plan rejected: <reason>"

### 3. 用户响应

PlanReview modal 在的时候，键位：

| 键 | 动作 |
|---|---|
| `y` / `Enter` | `action_tx.send(UserAction::ConfirmPlan)`；modal 暂不弹掉，等收到 `PlanConfirmed` 事件后弹 |
| `n` / `Esc` | 弹个迷你输入框收 reason？**不**——本步简化：直接发 `UserAction::RejectPlan("user rejected".into())`，不收 reason |
| `e` | 把 plan_text 复制到输入框（`PromptInputState.buffer`、`mode = Prompt`），关闭 modal，让用户改完后正常发送 |

### 4. 删除旧 hack

删除 Goal 143 迁移过来的代码：

- `app.rs` / 旧 `main.rs` 中识别 `"plan:"` 前缀进入 PlanReview 的逻辑
- 旧 `AppScreen::PlanReview` 枚举变体（如果还存在）—— 现在统一走 modal
  栈，`AppScreen` enum 可能能简化为 `Splash` / `Chat` 两态

### 5. 双击 Esc / Ctrl+C

新增状态：

```rust
pub struct DoublePressTracker {
    last_esc_at: Option<std::time::Instant>,
    last_ctrl_c_at: Option<std::time::Instant>,
}
const DOUBLE_PRESS_WINDOW: Duration = Duration::from_millis(2000);
```

#### Ctrl+C 行为

- 第一次按：
  - 如果有正在运行的 turn（`TurnState.running == true`）→ 发
    `UserAction::Interrupt`（backend 收到时调 `AgentRuntime` 的 cancel
    机制；如果 runtime 没原生支持 cancel，最小方案是给 worker 设个
    `cancel_flag: Arc<AtomicBool>`，runtime 通过自定义 EventSink 检查
    并返回某个错误 —— **本 goal 允许在 backend 层做 cancel，不要求改
    runtime trait**）
  - 如果没有运行 turn → 直接走"退出确认"路径
  - 在 transcript push System 块 "Interrupting… (press Ctrl+C again to
    exit)"
- 2 秒内再次按：`should_quit = true`
- 超过 2 秒：counter 重置

#### Esc 行为

- 第一次按：
  - 如果有 modal → 关闭顶层 modal
  - 否则如果 `buffer` 非空 → 清空 buffer
  - 否则如果 `TurnState.running == true` → 同 Ctrl+C 第一次
  - 否则什么都不做（不退出！这是和旧行为最大差异）
- 2 秒内再次按 + 上述都不适用：仍然不退出。Esc 不再是退出键。
- 退出统一走 `Ctrl+C×2`、`Ctrl+D`（input 空时）、`/exit`、`q`（仅在
  modal 中）

### 6. Backend 中断实现

```rust
// 在 backend.rs 中
pub struct Backend {
    pub action_tx: ...,
    pub event_rx: ...,
    cancel_flag: Arc<AtomicBool>,
    _worker: JoinHandle<()>,
}

// worker 任务里：
// SendMessage(text) =>
//     cancel_flag.store(false, Ordering::SeqCst);
//     let result = tokio::select! {
//         r = runtime.run(text) => r,
//         _ = wait_for_cancel(cancel_flag.clone()) => Err(...),
//     };
//
// Interrupt =>
//     cancel_flag.store(true, Ordering::SeqCst);
```

`wait_for_cancel` 是个简单的 poll 循环：每 100ms 检查 flag，true 就返回。
这不能真的让 LLM 请求中途断开（reqwest 不支持外部 cancel），但能让
TUI 立即回到响应状态，下一个 tool 调用前会被 abort。**够用即可，不追求
完美**。

如果觉得 `tokio::select!` 干净的方案是把整个 `runtime.run` 包到一个
`tokio::spawn` 里然后 abort，这也行（要注意 transcript 状态可能不一致）。

### 7. 测试

- `app::plan_proposed_event_opens_plan_review_modal`
- `app::plan_confirmed_closes_modal_and_pushes_system_block`
- `app::plan_rejected_pushes_system_block_with_reason`
- `app::plan_review_y_dispatches_confirm_plan_action`
- `app::plan_review_n_dispatches_reject_plan_action`
- `app::plan_review_e_copies_text_to_input_and_closes_modal`
- `app::esc_first_press_closes_modal_not_quits`
- `app::esc_first_press_clears_input_when_modal_empty_and_buffer_set`
- `app::esc_does_not_quit_after_double_press_when_idle`
- `app::ctrl_c_first_press_during_turn_dispatches_interrupt`
- `app::ctrl_c_first_press_idle_pushes_warning_then_exits_on_second`
- `app::ctrl_c_double_press_within_window_quits`
- `app::ctrl_c_outside_window_resets_counter`
- `backend::interrupt_action_sets_cancel_flag`
- `backend::run_with_cancel_flag_true_returns_quickly`（用 MockProvider
  跑长 turn 验证可中断）

### 8. 不做的事

- ❌ Reject 时收用户填的 reason（"user rejected" 字面量）
- ❌ 真正取消正在飞的 LLM HTTP 请求
- ❌ 中断历史 / 重做（撤销最近一条消息）
- ❌ Plan 编辑后 inline diff（`e` 是把文本扔回输入框，简单粗暴）

## Acceptance

1. `cargo test --workspace` 全绿
2. `cargo clippy --all-targets --all-features -- -D warnings` 无警告
3. `cargo fmt --all -- --check` 通过
4. 手工冒烟（依赖一个支持 planning_mode 的真实 LLM 或 mock）：
   - 开启 `/plan on`，发 "重构 X 模块"，能看到 PlanReview modal 弹出，
     里面列出 plan_text 和待执行 tool_calls
   - 按 `y` 后能看到 "Plan approved" 系统消息，agent 继续执行
   - 重新触发，按 `n`，"Plan rejected: user rejected"
   - 重新触发，按 `e`，plan 文本出现在输入框，可编辑后 Enter 发送
   - 跑一个长 turn，按 `Ctrl+C` 一次能让 spinner 停下并出现 "Interrupting..."
     系统消息，再按一次（2 秒内）退出
   - 按 `Esc` 当 buffer 非空时清空 buffer，buffer 空且无 modal 时无效果
     —— **不会退出**
5. 输入 `Plan: refactor X` 这种文本（旧 hack 的触发条件）**不再**弹
   PlanReview，而是作为普通消息发出
6. Goal 143/144/145/146 现有行为不回归

## Notes for the agent

- 在 `src/runtime.rs` 中找 `confirm_plan` / `reject_plan` 方法体，看
  reject 是如何 inject `Plan rejected: <reason>` 到 transcript 的
- `AgentEvent::PlanProposed.tool_calls` 字段类型是
  `Vec<recursive_agent::llm::ToolCall>`，渲染时取 `name` 与 `arguments`
- 双击窗口 2000ms 是个体感参数；可以读环境变量 `RECURSIVE_TUI_DOUBLE_MS`
  覆盖（默认 2000），方便测试
- `tokio::select!` 配合 `cancel_flag` 时小心 worker 死锁：用 `mpsc::Sender`
  发 cancel signal 比 atomic 更优雅，但 atomic 简单，本 goal 选择
  atomic + 100ms poll
- 测试 `interrupt` 行为时，给 MockProvider 配一个会 sleep 5s 的 hook，
  然后 spawn worker 跑 send + 1s 后发 interrupt，断言 1.5s 内任务结束
- 删 `"plan:"` 前缀 hack 时，搜全代码确认没有遗漏（`grep -n "starts_with(\"plan"`）
- 跨 goal 的 `Modal::PlanReview` 是 Step 4 显式延后到本步加的；不要忘
  了在 `ui/modal.rs` 把它加进 enum 并接入 dispatch

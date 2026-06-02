# Goal 202 — Plan Mode Pre-Confirmation

**Roadmap**: TUI UX Phase — Plan Flow 体验优化

**Design principle check**:
- 新建 `src/tools/plan_mode.rs` 中 `RequestPlanModeTool` + `PlanModeRequestGate`
- 新建 `src/event.rs` 中 `AgentEvent::PlanModeRequested`
- TUI 侧仅在 `src/tui/app.rs` 和 `src/tui/ui/transcript.rs` 添加处理
- ❌ 不修改 `agent.rs` 主循环
- ❌ 不改动已有 `enter_plan_mode` / `exit_plan_mode` 工具语义

---

## Why

当前 plan mode 流程存在顺序倒置问题：

```
现状：
  用户提问 → LLM 自行决定进入 plan mode → 生成完整计划 → 展示给用户审批
               ↑  浪费 token（整个计划白写）如果用户根本不想要计划

期望：
  用户提问 → LLM 请求进入 plan mode（说明原因）→ 用户 [y/n] → 
               ├─ y: 进入 plan mode → 探索代码 → 生成计划 → 用户审批
               └─ n: 直接执行（无探索、无计划浪费）
```

已经完成的 Goal E（inline plan display）处理了"计划生成后的审批体验"，
本 Goal 处理"是否需要计划"这个前置决策，让用户掌握主动权。

---

## Scope (do exactly this, no more)

### 1. `src/event.rs` — 新增 `PlanModeRequested` 事件

```rust
// 在 AgentEvent enum 中添加
AgentEvent::PlanModeRequested {
    /// 为什么需要 plan mode（LLM 提供的原因）
    reason: String,
},
```

同时在 `AgentEvent::PlanConfirmed` / `PlanRejected` 对称地新增：

```rust
AgentEvent::PlanModeApproved,
AgentEvent::PlanModeRejected { reason: String },
```

### 2. `src/tools/plan_mode.rs` — 新增 `PlanModeRequestGate` + `RequestPlanModeTool`

```rust
/// Gate for the pre-confirmation step (before plan exploration starts).
/// Parallel to PlanApprovalGate but for the entry decision.
pub struct PlanModeRequestGate {
    response: Arc<RwLock<Option<PlanModeRequestResult>>>,
    notify:   Arc<Notify>,
}

#[derive(Debug, Clone)]
pub enum PlanModeRequestResult {
    Approved,
    Rejected { reason: String },
}

impl PlanModeRequestGate {
    pub fn new() -> Self { ... }
    pub async fn wait_for_decision(&self) -> PlanModeRequestResult { ... }
    pub fn approve(&self) { ... }
    pub fn reject(&self, reason: impl Into<String>) { ... }
}
```

新工具 `RequestPlanModeTool`：

- tool name: `request_plan_mode`
- description: `"Before entering plan mode, call this tool to request the user's \
  permission to enter planning mode. Provide a brief reason why planning is helpful \
  for this task. Blocks until the user approves or rejects."`
- parameters: `{ "reason": { "type": "string" } }` (required)
- execute:
  1. emit `AgentEvent::PlanModeRequested { reason }`
  2. `wait_for_decision().await`
  3. 返回 `{"approved": true}` 或 `{"approved": false, "reason": "..."}`

**注意**：LLM 收到 `{"approved": false}` 后应直接执行而不进入 plan mode。
这由工具返回值驱动，不需要修改 agent 主循环。

### 3. `src/runtime.rs` — 注册工具 + 暴露决策 API

```rust
// AgentRuntimeBuilder::build() 中注册新工具
registry.register(RequestPlanModeTool::new(
    plan_mode_request_gate.clone(),
    event_sink.clone(),
));

// AgentRuntime 新增两个方法
pub fn approve_plan_mode_request(&self) { ... }
pub fn reject_plan_mode_request(&self, reason: &str) { ... }
```

`PlanModeRequestGate` 与 `PlanApprovalGate` 分开管理，不共享状态，
避免并发使用时互相干扰。

### 4. `src/tui/app.rs` — 处理新 UiEvent + UserAction

新增 `UiEvent` 变体（在 `src/tui/events.rs` 或就地）：

```rust
UiEvent::PlanModeRequested { reason: String }
UiEvent::PlanModeApproved
UiEvent::PlanModeRejected { reason: String }
```

新增 `UserAction` 变体：

```rust
UserAction::ApprovePlanMode
UserAction::RejectPlanMode(String)
```

新增 `TranscriptBlock` 变体（inline 展示请求）：

```rust
TranscriptBlock::PlanModeRequest {
    reason: String,
    /// true after user approved/rejected (to show final state)
    decided: bool,
    approved: Option<bool>,
}
```

App 状态新增字段：

```rust
pub plan_mode_request_pending: bool,  // true when awaiting user decision
```

`handle_ui_event` 处理 `UiEvent::PlanModeRequested`：
- push `TranscriptBlock::PlanModeRequest { reason, decided: false, approved: None }`
- `plan_mode_request_pending = true`

键盘处理（当 `plan_mode_request_pending` 时，`y`/`n` 被拦截）：
- `y` → `UserAction::ApprovePlanMode`，`plan_mode_request_pending = false`
- `n` / `Esc` → `UserAction::RejectPlanMode("user rejected")`，`plan_mode_request_pending = false`

### 5. `src/tui/ui/transcript.rs` — 渲染 `PlanModeRequest` block

```
┌─ ⓘ Plan Mode Request ──────────────────────────────────────
│ Agent wants to enter plan mode:
│
│   "这个任务涉及多个文件，我想先探索代码再执行变更"
│
│ Allow agent to explore and create a plan?
│
│  [y/Enter] Allow    [n/Esc] Skip — execute directly
└────────────────────────────────────────────────────────────
```

决策完成后更新 block 显示状态（`decided: true`）：
- Approved: 显示 `✓ Plan mode approved`（绿色）
- Rejected: 显示 `✗ Plan mode skipped`（灰色）

### 6. `src/tui/ui/chat.rs` — 批准横幅

当 `plan_mode_request_pending` 时，在状态栏和输入框之间显示 1 行横幅
（复用 Goal E 的 `plan_banner_height` 机制）：

```
 ⓘ Plan mode request — [y/Enter] Allow   [n/Esc] Skip
```

与 Plan Approval Banner 样式相似，区分颜色（蓝色主调 vs 黄色）。

### 7. `src/tui/backend.rs` — 响应 UserAction

```rust
UserAction::ApprovePlanMode => {
    runtime.approve_plan_mode_request();
}
UserAction::RejectPlanMode(reason) => {
    runtime.reject_plan_mode_request(&reason);
}
```

### 8. HTTP API — `POST /sessions/:id/plan_mode_decision`

（与现有 `plan_decision` 端点对称，字段 `approve: bool`）

```json
POST /sessions/:id/plan_mode_decision
{ "approve": true }
```

---

## 单元测试

- `request_gate_approve_wakes_waiter`
- `request_gate_reject_propagates_reason`
- `request_gate_cleared_after_use`
- `request_plan_mode_tool_emits_event_and_blocks`
- `plan_mode_request_pending_set_on_event`
- `plan_mode_request_y_clears_pending_and_dispatches_action`
- `plan_mode_request_n_clears_pending_and_dispatches_reject`

---

## Acceptance

- `cargo test --workspace` 绿色（含上述 7 个测试）
- `cargo clippy --all-targets -- -D warnings` 干净
- `cargo fmt --all` 无变化
- TUI 中：LLM 调用 `request_plan_mode` 时，transcripts 出现请求 block
- 用户按 `n` 后 LLM 不进入 plan mode，block 变为 `✗ Plan mode skipped`
- 用户按 `y` 后 LLM 进入 plan mode，后续 `exit_plan_mode` 流程不变
- HTTP API 端点测试：`plan_mode_decision` 正确唤醒 gate

---

## Notes for the agent

- `RequestPlanModeTool` 命名遵循现有 `EnterPlanModeTool` 规范
- `PlanModeRequestGate` 与 `PlanApprovalGate` 内部结构相同，
  但分开定义避免语义混淆（入场确认 vs 计划审批）
- `request_plan_mode` 工具是**可选的**：未注册时 LLM 不可见，不影响现有行为
- 若 `plan_mode_request_pending` 和 `plan_awaiting_approval` 同时为 true，
  以 `plan_awaiting_approval` 优先（但正常流程下不会同时出现）
- HTTP `plan_mode_decision` 端点与现有 `plan_decision` 端点共享鉴权但互相独立
- `TranscriptBlock::PlanModeRequest` 的 `decided/approved` 字段允许在事后
  渲染历史记录时正确显示决策结果（"当时选了 y 还是 n"）

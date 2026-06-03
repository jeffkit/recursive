# Goal 219 — Refactor: 删除 deprecated Agent/StepEvent/AgentOutcome（breaking）

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: 无（v0.7 起点；前置 v0.6 阶段的 AgentRuntime 必须已落地，✅ Goal 200+）
**类型**: A — 架构级重构（人/Claude 主导，非 self-improve）

## Why

`.dev/AGENTS.md` 第 1 条 invariant 明确 "agent loop stays small"，但当前 `src/agent.rs` 2837 行，仍保留完整的 `Agent`/`AgentOutcome`/`StepEvent`/`OnMessageFn` 旧实现作为 `#[deprecated]` 转发。这条 invariant 自 0.5.0 写起，到 0.6.0 都没真正落地。

## 修订：拆 2 个独立 commit

2026-06-03 重新评估后，219 实际是**两个独立工作的合集**，拆 2 次提交以降低单次 commit 风险 + 便于回滚：

- **Commit 1** (Steps 1-3): `refactor(kernel): migrate RunCore to emit AgentEvent directly`
  - `RunCore::events` 类型从 `Sender<StepEvent>` 改为 `Sender<AgentEvent>`
  - 16 处 `emit(StepEvent::Foo)` 改为 `emit(AgentEvent::Foo)`
  - `AgentKernel::run` 删掉内部 bridge
  - 旧 `Agent` / `StepEvent` 仍存在但**已无人调用**（dead code，留给 Commit 2 清）
  - 中间状态：build/test 全绿
- **Commit 2** (Steps 4-6): `refactor: delete deprecated Agent/StepEvent/AgentOutcome (BREAKING)`
  - 删 `StepEvent`/`OnMessageFn`/`Agent`/`AgentBuilder`/`AgentOutcome`
  - 删 `event.rs` 的 bridge、`cost.rs` 的 `on_message_callback`
  - 改 `HookEvent` 字段从 `&AgentOutcome` 改 `&RuntimeOutcome`
  - 8 处集成测试 `Agent::builder()` → `AgentRuntime::builder()`
  - 删 `lib.rs` 5 个 deprecated `pub use`
  - 移除所有 `#[allow(deprecated)]`

**为什么拆 2 个 commit：** Commit 1 之后 `Agent` 还在但已无人调用，是个"安全的死代码"状态——回滚 Commit 1 比回滚"删 Agent 同时改 RunCore 类型"容易得多。Commit 2 才是真正的 SemVer breaking change。

实际危害：

- `lib.rs:51-55` 同时 re-export 两套 API（`Agent` + `AgentRuntime`），下游调用方分裂
- `run_core.rs`（766 行）作为 `Agent::run` 的内部算法，被 `#[allow(deprecated)]` 包起来继续走旧路径
- `tui/events.rs` 等下游 bridge 仍然监听 `StepEvent` 而不是 `AgentEvent`
- 任何对循环语义的改动都要双写两套

v0.7 是收尾 release。版本号从 0.6.0 → 0.7.0 即允许 breaking change，趁机清干净。

## Design

### 保留并迁移（4 个类型）

| 类型 | 当前位置 | 目标位置 | 理由 |
|---|---|---|---|
| `FinishReason` | `agent.rs:220` | `agent/types.rs`（新文件） | 仍被 `AgentRuntime`、`TurnOutcome` 使用 |
| `PermissionDecision` | `agent.rs:32` | `agent/types.rs` | 仍被 permission hook 使用 |
| `PermissionHook` | `agent.rs:49`（type alias） | `agent/types.rs` | 同上 |
| `PlanningMode` | `agent.rs:66` | `agent/types.rs` | 仍被 `AgentRuntime`/`AgentKernel` 使用 |

`agent/types.rs` 体量应在 100-150 行之间（一个枚举、一个 type alias、两个 enum，全是纯类型 + 文档注释）。

### 删除（5 个类型 + 1 个文件）

| 类型/文件 | 行数 | 删除原因 |
|---|---|---|
| `Agent` | `agent.rs:281-731` | 完全被 `AgentRuntime` 取代 |
| `AgentBuilder` | `agent.rs:732-749` | 同上 |
| `AgentOutcome` | `agent.rs:256-263` | 同上 |
| `OnMessageFn` | `agent.rs:60`（type alias） | `EventSink` 已取代 |
| `StepEvent` | `agent.rs:88-202` | `AgentEvent` 已取代 |
| `src/run_core.rs` | 766 行 | **修订：不能简单删除**。`AgentKernel::run` (kernel.rs:301) 也用 `RunCore`。正确做法：把 `RunCore::events` 类型从 `Sender<StepEvent>` 改成 `Sender<AgentEvent>`，把 16 处 `emit(StepEvent::Foo)` 替换为 `emit(AgentEvent::Foo)`（注意 `Finished` → `TurnFinished` 重命名），保留 `run_core.rs` 作为 kernel 内部模块。 |

### 改写下游

| 文件 | 改动 |
|---|---|
| `src/lib.rs:51-55` | 删除 5 个 deprecated `pub use`；保留 4 个 keeper 的 `pub use`（改为从 `agent::types`） |
| `src/kernel.rs:28` | `use crate::agent::{FinishReason, PermissionHook, PlanningMode}` → `use crate::agent::types::{...}` |
| `src/kernel.rs:280-348` (`AgentKernel::run`) | 删 line 286-298 的内部 StepEvent 桥接；`core_events_tx` 改 `Sender<AgentEvent>` 直接传给 `RunCore` |
| `src/runtime.rs:25` | keeper 路径更新 |
| `src/event.rs:24, 213-306, 432` | 删 `use crate::agent::StepEvent`；删 `impl From<StepEvent> for AgentEvent`（90 行 bridge 现在是 dead code）；测试里 `FinishReason` import 改路径 |
| `src/hooks/mod.rs:3, 50, 110, 120, 132, 462-471, 546, 664` | 删 `#[allow(deprecated)]`（line 3）；删 `use crate::agent::AgentOutcome`；`HookEvent::SessionEnd/Stop/SubagentStop` 的 `&AgentOutcome` 改 `&RuntimeOutcome`；测试构造 `AgentOutcome` 处改 `RuntimeOutcome` |
| `src/hooks/external.rs:30, 511, 528, 530` | `use crate::agent::StepEvent` → `use crate::event::AgentEvent`；`StepEvent::Hook*` → `AgentEvent::Hook*`（字段映射在 event.rs:213 bridge 里） |
| `src/cost.rs:252-263` | 删 `on_message_callback` 方法（返回的 `OnMessageFn` 已删，且方法体是 no-op 桩） |
| `src/run_core.rs` | 16 处 `emit(StepEvent::Foo)` → `emit(AgentEvent::Foo)`（`Finished` → `TurnFinished`）；`RunCore::events` 类型改 `Sender<AgentEvent>`；`use` 行去掉 `StepEvent`、`OnMessageFn`；删 line 7 的 `#![allow(deprecated)]` |
| `src/agent.rs` | 迁出 `FinishReason`/`PermissionDecision`/`PermissionHook`/`PlanningMode` 到新 `src/agent/types.rs`；删 `Agent`/`AgentBuilder`/`AgentOutcome`/`OnMessageFn`/`StepEvent`；文件最终形式：`pub mod types;` + `pub use types::*;` + 文件头注释 |
| 新文件 `src/agent/types.rs` | 容纳 4 个 keeper 类型（一个 enum、一个 type alias、两个 enum），约 150 行 |
| `src/tools/spawn_worker.rs:26`, `src/tools/sub_agent.rs:25`, `src/multi.rs:3` | keeper 路径改 `use crate::agent::types::{...}` |
| `tests/smoke.rs:73`, `tests/anthropic_smoke.rs:99`, `tests/integration.rs:6, 157, 329, 443, 515, 581, 669, 914` | `Agent::builder()` → `AgentRuntime::builder()`；`agent.run(goal).await?` → `runtime.run(goal).await?`；变量名改 `runtime`；移除 `tests/integration.rs:6` 的 `// Allow deprecated` 注释 |
| `examples/with_hooks.rs:3` | 改注释 |

### 测试迁移

集成测试里如果有 `Agent::builder()...run()` 模式，全部改为 `AgentRuntime::builder()...run()`。预期改动：

- `tests/integration.rs`
- `tests/smoke.rs`
- `tests/mcp_integration.rs`
- `tests/v050_integration.rs`
- `tests/v060_storage_integration.rs`
- `tests/anthropic_smoke.rs`
- `tests/orphan_resume.rs`
- `tests/resume_by_id.rs`

具体条目需要在 worktree 里 `grep -l "Agent::builder\|StepEvent::\|AgentOutcome" tests/` 重新确认。

## 验收标准

- `agent.rs` ≤ 200 行（只剩 `pub mod types; pub use types::*;` 几行 + 文件头注释）
- `run_core.rs` 已删除
- `grep -rn "Agent::builder\|StepEvent::\|AgentOutcome\|OnMessageFn" src/ tests/` 返回 0
- `grep -rn "#\[allow(deprecated)\]" src/` 返回 0（不允许再用 `#[allow(deprecated)]` 蒙混）
- `cargo test --workspace` 全绿
- `cargo clippy --all-targets --all-features -- -D warnings` 无警告
- `cargo fmt --all` 无 diff
- `src/agent.rs` 不再被 `runtime.rs` / `kernel.rs` 之外的下游 import 任何 runtime 类型
- 一篇 journal entry 记录迁移中遇到的 non-obvious 决策

# Goal 219 — Refactor: 删除 deprecated Agent/StepEvent/AgentOutcome（breaking）

**Roadmap**: Phase 20 — 0.7 Refactor & Hardening
**依赖**: 无（v0.7 起点；前置 v0.6 阶段的 AgentRuntime 必须已落地，✅ Goal 200+）
**类型**: A — 架构级重构（人/Claude 主导，非 self-improve）

## Why

`.dev/AGENTS.md` 第 1 条 invariant 明确 "agent loop stays small"，但当前 `src/agent.rs` 2837 行，仍保留完整的 `Agent`/`AgentOutcome`/`StepEvent`/`OnMessageFn` 旧实现作为 `#[deprecated]` 转发。这条 invariant 自 0.5.0 写起，到 0.6.0 都没真正落地。

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
| `src/run_core.rs` | 766 行 | 仅服务于 `Agent::run` 的旧循环 |

### 改写下游

| 文件 | 改动 |
|---|---|
| `src/lib.rs:51-55` | 删除 5 个 deprecated `pub use`；保留 4 个 keeper 的 `pub use`（改为从 `agent::types`） |
| `src/kernel.rs:28` | `use crate::agent::{FinishReason, PermissionHook, PlanningMode}` → `use crate::agent::types::{...}` |
| `src/runtime.rs:25` | 同上 |
| `src/event.rs` | 检查是否仍 import `StepEvent`；如有，改为只 export `AgentEvent` |
| `src/tui/events.rs` | 把 `StepEvent::Foo` 订阅改为 `AgentEvent::Foo`（可能已经在做，验证即可） |
| `src/agent.rs:62, 300-301, 303-731, 749-` | 整文件重写为 `pub mod types;` + `pub use types::*;` 的薄壳 |
| `src/run_core.rs` | 整个文件删除 |
| `src/mcp.rs:1-50` | 若有 `StepEvent` bridge 同样改写 |

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

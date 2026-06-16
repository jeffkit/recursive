# Manual edit: arch-review-improvements

**Date**: 2026-06-16
**Goal**: 对 agent 核心架构进行深度 review 后，将发现的 bug 和可改进点转化为 Goal，由 Recursive 自我迭代完成修复
**Files touched**:
- `.dev/goals/285-fix-denial-sentinel-double-push.md` (new)
- `.dev/goals/286-stuck-detection-accurate-tool-name.md` (new)
- `.dev/goals/287-llm-retry-event.md` (new)
**Tests added**: 由各 Goal 的 self-improve run 自动添加
**Notes**:

## 架构 Review 结论

对 `run_core.rs` / `kernel.rs` / `runtime.rs` / `compact.rs` / `error.rs` 做了深度阅读，
识别出以下问题（按严重程度）：

### 🔴 Bug（已修复）
- **C2 — DENIAL_LIMIT_SENTINEL 双重 push（Invariant #8 违反）**
  外层 for 循环已 push 了 sentinel 之前的 tool results，内层 flush 循环再次 push 所有，
  导致 transcript 中同一 tool_call_id 出现两次，下次 LLM 调用将触发 HTTP 400。
  → Goal 285 修复，同时发现并修复关联 bug：`PermissionDeniedLimit` 被错误包成 `Error::Tool`。

### 🟡 正确性改进（已修复）
- **C1 — Stuck 检测报告的 repeated_call 语义不准**
  `FinishReason::Stuck.repeated_call` 报告的是"触发检测时当前迭代的工具名"，
  而非"窗口内最频繁出错的工具"。
  → Goal 286 将 `VecDeque<bool>` 改为 `VecDeque<(bool, String)>`，用频率统计选出正确工具名。

### 🟢 可观测性改进（已实现）
- **O2 — LLM retry 时无 AgentEvent**
  限流/超时 backoff 时只有 tracing log，TUI 和 SDK 消费者无法感知正在重试。
  → Goal 287 新增 `AgentEvent::LlmRetry { step, attempt, wait_ms, reason }`，
    在 `call_llm_with_retry` backoff 前 emit。

## 其余改进项（未本次实施，留待后续）
- P1: 每 turn 全量 clone transcript（需 COW 或 delta 设计，改动大）
- P3: `ToolRegistry` 每 turn clone + Arc 包（需引入 `Arc<ToolRegistry>` cache）
- D1: Plan Mode 三个分散同步原语（状态机重构，范围大）
- C3: cross-turn compact 在 user message 之后立即触发（边界测试缺失）
- C4: 双层 retry 未文档化

## Self-improve 运行统计
- Goal 285: 125 steps, $0.12, deepseek-v4-pro
- Goal 286: ~50 steps（快速，机械改动）
- Goal 287: 287 steps, $0.17, deepseek-v4-pro（测试 fix 需多步）
- 总耗时: ~40 分钟（串行，因三个 Goal 均修改 run_core.rs）
- 所有 Goal verdict: committed ✅

# Manual edit: quality-fixes-r5

**Date**: 2026-06-22
**Goal**: 修复第5轮代码审查发现的3个问题（1个MEDIUM，2个LOW）
**Files touched**:
- `src/tools/plan_mode.rs`
- `src/mcp.rs`
- `src/http/rate_limit.rs`

**Tests added**: none（现有测试全部通过）

## 修复详情

### MEDIUM-1: PlanApprovalGate 幽灵审批竞态（plan_mode.rs）

**问题**：`ExitPlanModeTool::execute` 直接写 `pending_plan`，但未清除上轮残留的 `response`。若两个并发 HTTP 请求同时通过 `session_plan_confirm` 的 `pending.is_none()` 检查（两者都持有共享读锁），第二个请求的 `approve()` 调用会把 `response=Some(Approved)` 留在里面。下一次 `wait_for_approval()` 循环开始时立即读到这个幽灵值，Agent 跳过人类审批直接通过。

**修复**：在 `PlanApprovalGate` 中新增 `begin_approval(plan_text)` 方法，原子性地同时写 `pending_plan` 和清除 `response`（两个 `RwLock` 顺序写，同步函数无需跨 await）。将 `ExitPlanModeTool::execute` 中的直接写替换为 `self.gate.begin_approval(plan_text.clone())`。

这个修复覆盖了"竞态 B → 幽灵审批"场景：即使残留的 `response` 被写入，下一轮 `begin_approval()` 调用会在 `wait_for_approval()` 开始之前将其清零。

### LOW-1: mcp.rs 超时错误消息硬编码 "10s"（mcp.rs）

**问题**：`read_stdio_response` 接受可配置的 `read_timeout: Duration`，但错误消息写死了 "10s"，当调用方传入不同超时值时消息误导。

**修复**：改为 `format!("server timed out (no response within {}s)", read_timeout.as_secs())`。

### LOW-3: 零值 rate limiter 配置无警告（rate_limit.rs）

**问题**：`rate_limiter_from_env()` 接受 `RECURSIVE_RATE_LIMIT_RPM=0` / `BURST=0` 但无任何日志，会静默造成所有请求被拒绝或令牌桶永不补充。

**修复**：在解析后 `RateLimiter::new` 之前增加两个 `tracing::warn!` 条件检查。

## 验证

```
cargo fmt --all     → OK
cargo clippy --all-targets --all-features -- -D warnings  → OK（0 warnings）
cargo test --workspace  → OK（所有测试通过）
```

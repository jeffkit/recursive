# Goal 409 — exit_plan_mode 审批等待接入取消令牌 + 超时默认值可配置/等待提示（#48 复验 follow-up）

**Roadmap**: issue #48 复验（2026-09-30，comment 5902987299）确认 #47④ 的 300s
超时修复有效（turn 不再永久 park），但复验同时实证一处措辞更正：Ctrl-C 在
44f399d（Goal 407）之后依然**延迟生效**——`shutdown:` 行立刻打印（信号已记录），
turn 却要等 300s 超时释放 plan gate、回到 step 边界才真正停下。本 goal 落地该
复验的建议 1、2；建议 3（set_event_sink 链路集成断言）已由
`tests/integration.rs::approval_timeout_lets_turn_finish_with_rejection` 覆盖，不在本 goal 范围。

**依赖**: #47④（d693ff9，bounded approval wait + REPL 300s）；Goal 407（44f399d，
REPL per-turn 令牌 slot——本 goal 消费的就是这个令牌）。

**Design principle check**:
- ✅ 消灭「不可中断 await」这最后一类：取消检查目前只覆盖 step 边界与 LLM 调用处
  （`src/run_core.rs` 自陈），工具内部的 await 是盲区；把令牌 select 进审批等待后，
  wait-forever 宿主（TUI / SDK，不设 300s）同样能被 Ctrl-C 救出。
- ❌ Does NOT 在 `run_inner` 加分支（invariant #1）——改动落在 `src/tools/plan_mode.rs`
  与工具构造/重注册处。
- ❌ Does NOT 破坏 finish-is-data（invariant #7）——令牌触发路径返回
  `PlanApprovalResult::Rejected { reason }` 数据，不是 Err。
- ❌ Does NOT 把 300s 换成另一个硬编码——只把默认值改成可配置；更小的默认数值属
  产品决策，本 goal 只交付配置面与提示行。

## Why（2026-09-30 复验实证）

REPL 宿主设了 300s：用户 Ctrl-C 后仍要干等到超时（用例 B：31s 发 Ctrl-C、316s 才
返回，脱身靠的是超时不是令牌）。若宿主不设超时（TUI/SDK 的 wait-forever 语义），
Ctrl-C 完全救不了这个 await。且 5 分钟「看起来冻住」的 REPL 没有任何等待提示，
体验上与真死机无法区分（triage 判据 manual-20260929-issue47 的全部三条都要跑一遍
才能排除）。

## Scope（do exactly this, no more）

**Touches**: `src/tools/plan_mode.rs`, `src/runtime.rs`（重注册路径）, `src/run_core.rs`
（仅令牌传递，不加分支）, `crates/recursive-cli/src/main.rs`（REPL 配置读取 + 提示行）。

### 1. 令牌接入审批等待（主体）
- `ExitPlanModeTool` 持有可选取消令牌；等待处 `tokio::select!`：
  `r = gate.wait_for_approval() => r, _ = token.cancelled() => Rejected { reason: "cancelled while waiting for plan approval" }`。
- 令牌来源与 Goal 407 的 REPL per-turn slot 对齐（turn 开始安装 child、结束清空）；
  `AgentRuntime::set_event_sink` 重注册 `ExitPlanModeTool` 时（`src/runtime.rs:937-943`
  一带）同步携带，避免 sink 交换把带令牌的工具换回无令牌版本。
- 令牌触发路径必须走 `gate.reject(...)` 同一条清理路径，保证 `pending_plan` 清空
  （防 compaction 复活，同 #47④ 的超时分支）。

### 2. 超时默认值可配置 + 等待提示
- REPL 的 300s 改为可配置（env，如 `RECURSIVE_PLAN_APPROVAL_TIMEOUT_SECS`，0 =
  恢复 wait-forever 语义）；默认值维持 300s 不变（改默认数值是产品决策，另行讨论）。
- 发出 `PlanProposed` 进入等待前输出一行提示（如
  `waiting for plan approval — Ctrl-C to cancel`），消除「假死」观感。

### 3. 测试（与实现同 commit，契约非建议）
- 令牌取消 ⇒ `exit_plan_mode` 立即以 `"approved":false` 返回（不等超时），
  `pending_plan` 清空。
- **无超时 + 令牌取消** ⇒ 同样立即脱身（这是对 TUI/SDK wait-forever 宿主的关键
  收益，必须有独立断言钉住）。
- 既有链路保持绿：`wait_for_approval_times_out_and_rejects`
  （src/tools/plan_mode.rs）、`approval_timeout_lets_turn_finish_with_rejection`
  （tests/integration.rs:1266）。
- 既有门不破：`cargo test --workspace`、`cargo clippy --all-targets --all-features
  -- -D warnings`（对改动 crate 强制重 lint）、`cargo fmt --all`。

## 验收（acceptance）
1. 等待期内任何令牌取消源（REPL Ctrl-C / per-turn slot 清理）立刻终止等待，
   turn 以数据形式收尾。
2. 无超时宿主同样可取消（wait-forever ≠ 不可中断）。
3. 等待期有提示行。
4. 全量门 + main CI 绿。

# Manual edit: goal-353 — mid-stream cancellation must persist the transcript (Invariant #7)

**Date**: 2026-08-01
**Goal**: 把 mid-stream `Error::Cancelled`（LLM 流在 SSE chunk 之间被 shutdown token 打断）
从「`?` 冒泡成 `Err`」改为「翻译成 `FinishReason::Cancelled` outcome」，让 caller
（runtime.rs）正常持久化部分 transcript、跑 `emit_turn_messages` + compaction、
并激活 `SessionEnd` hook 的取消抑制。修复前 provider 注释声称 run_core 已有翻译逻辑，
实际 grep 零匹配——注释是假的。

## 根因

- `src/llm/anthropic.rs` / `src/llm/openai.rs` 的 stream 循环在 `tokio::select!`
  的 `ct.cancelled()` 分支 `return Err(Error::Cancelled)`。
- 该错误沿 `dispatch_llm_step`（run_core.rs:347）→ `run_inner` 的 `?` 一路冒泡，
  经 kernel.rs:346、runtime.rs:664 到 runtime.rs:284 的 catch-all `Err(e) => return Err(e)`，
  导致 `emit_turn_messages`（285）、`maybe_compact_cross_turn`（307）、
  `turn_index.fetch_add`（319）全部被跳过——部分 transcript 不落盘（Invariant #7 违规）。
- 唯一处理 Cancelled 的是 `check_shutdown`（run_core.rs:697），但它只在每步循环顶部
  （LLM 调用前）运行，mid-stream 的取消永远到不了那里。

## 改动

- **`src/run_core.rs`**：
  - 新增 sibling helper `make_cancelled_outcome(self, step, final_message, total_usage,
    tool_audits)`（紧邻 `make_outcome`）：组装 `FinishReason::Cancelled` outcome，
    发射 `AgentEvent::TurnFinished`（`reason`/`steps` 字段与 `check_shutdown` 完全一致），
    打 `agent.run.cancelled_mid_stream` tracing 日志。`finished_steps` 用 `step`
    （LLM 调用 in-flight），**不是** `check_shutdown` 的 `step - 1`（调用前）；helper
    不计算该值、由调用点显式传入，step-vs-step-1 区分在调用点可见（goal 要求不要统一）。
  - `run_inner` 的 LLM 调用点（原 `dispatch_llm_step(...).await?`）改成 3-arm match：
    `Ok(v) => v` / `Err(Error::Cancelled) => return Ok(self.make_cancelled_outcome(...))` /
    `Err(e) => return Err(e)`。非 Cancelled 错误仍按原 `?` 语义冒泡。
  - **Invariant #1（run_inner ≤150 行）**：inline 版本把函数体推到 157 行触发
    `loop_size_orthogonality` 硬门；按该测试自身建议把 mid-stream 分支抽成 sibling
    helper，函数体回到 142 行。helper 是**专用**的（check_shutdown 不用它），
    step/step-1 语义没有统一。
- **`src/llm/anthropic.rs` / `src/llm/openai.rs`**：只改注释——从假断言
  「run_core 已有逻辑翻成 FinishReason::Cancelled」改为可验证指针
  「run_inner 的 dispatch_llm_step 调用点会把这个错误翻译成 FinishReason::Cancelled」。
  `return Err(Error::Cancelled)` 行为未动（错误是正确的中层信号，翻译在 caller）。
- **`src/tools/agent.rs`**：顺带修掉一个**预先存在**的 clippy lint
  `useless_conversion`（`format!(...).into()` 的 `.into()` 对 `String` 字段冗余）——
  base commit 在 rust 1.97 下本身过不了 clippy 硬门，flow 会整单回滚，必须修。
- **fmt 噪声**：`cargo fmt --all`（rust 1.97）对 base 里用旧 rustfmt 排版的文件
  （config.rs / system_prompt.rs / tasks.rs / tools/task_output.rs / agent.rs 的换行）
  做了纯格式化归一，无逻辑改动。

## Tests added（src/run_core.rs `#[cfg(test)] mod tests`，复用 `make_run_core_for_inner` + `MockProvider`）

- **`cancelled_mid_stream_persists_transcript_and_finish_reason`**（HEADLINE）：
  mock provider `.with_errors(vec![Error::Cancelled])`（error 队列先于 scripted
  completions 消费 → 第一次 LLM 调用即返回 Cancelled）；提供**未取消**的
  shutdown token 确保不走 check_shutdown。断言：`Ok(outcome)`、
  `finish_reason == Cancelled`、`steps == 1`（循环首轮 step=1，mid-stream 用
  in-flight 的 step，注释说明为何不是 0）、`TurnFinished("cancelled", steps=1)`
  事件已发射、`outcome.messages` 保留种子消息（transcript 不丢）。
- **`cancelled_between_steps_uses_check_shutdown_path`**：token 在 run_inner 前已
  cancel → 走 check_shutdown（loop 顶部），断言 `Ok` + `Cancelled` + `steps == 0`；
  钉住既有路径，两条路径都覆盖且可区分。
- **`non_cancelled_error_still_bubbles`**：`.with_errors(vec![Error::Llm { .. }])`，
  断言 `run_inner` 返回 `Err(Error::Llm)`——新 match 只吞 Cancelled。

## 验证（全部通过）

- `cargo test --workspace` 全绿（含 3 个新测试 + `loop_size_orthogonality` 硬门）
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` 干净
- `cargo fmt --all -- --check` 干净
- `rg "Error::Cancelled" src/run_core.rs src/kernel.rs src/runtime.rs` →
  现在 `src/run_core.rs` 有匹配（新 catch arm + helper doc + 测试），
  之前死掉的 Cancelled 翻译路径已存在。

## Notes

- **`steps` 断言与 goal 文本的一处偏差**：goal 的 headline 测试写「`outcome.steps == 0`
  （cancellation on step 0's LLM call）」，但循环是 `for step in 1..=step_cap`，
  第一次 LLM 调用在 step=1，mid-stream arm 按 goal 自身 note 用 `step`（非
  `step - 1`）→ 实际值是 1。goal 的 `== 0` 与其 note 自相矛盾（疑似从
  check_shutdown 测试复制）；实现与测试按 note 的语义走（step vs step-1 区分），
  并加了注释说明。`cancelled_between_steps` 测试的 `steps == 0` 则成立
  （check_shutdown 用 `step.saturating_sub(1)`）。
- runtime.rs / kernel.rs 未动（goal 要求 verify by test, don't edit）——headline
  测试证明 `run_inner` 现在返回 `Ok(Cancelled)`，runtime 既有 Ok 分支自然生效。

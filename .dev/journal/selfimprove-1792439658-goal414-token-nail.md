# Goal 414 — 上下文与压缩契约（2/2 之 2）：token 归一钉子 + 降级可观测

**Date**: 2026-10-20
**Run**: selfimprove-1792439658（branch v2-selfimprove-1792439658）
**前置**: Goal 411（7ad601b Compactor::apply 单一路径）+ Goal 413（f20b31cb canonical rebuild）
**恢复来源**: 半成品快照 `wip-selfimprove-1792432500`（上一 run 死于 wall-clock 3600s），diff 评估后整体应用，未从零重写。

## Goal

- **token 归一钉子**（#162/#163）：压缩触发消费侧必须使用 adapter 归一后的
  `TokenUsage::input_tokens()`（cache 折算），未归一形状显式 WARNING 后按保守口径决策。
- **降级可观测**：摘要替换 / PTL 截断 / microcompact 占位符全部 WARNING 到
  `recursive::compact`（before/after 消息数、被摘要区间）；合法 no-op 保持 debug。
- 回归钉住 + 变异负控（删守卫/删 WARNING 必须红）。

## Files touched

- `src/llm/chat.rs` — `TokenUsage::input_tokens()`（`max(prompt_tokens, hit+miss)`，无 split 的 provider 原样保留）+ `TokenUsage::is_normalized()`（`split == 0 || split == prompt_tokens`）。单元测试 1 条（Anthropic / DeepSeek / 无 split 三形状）。
- `src/compact/mod.rs` — `Compactor::should_compact_usage(estimate_bytes, &TokenUsage)`：未归一时 WARNING（带 prompt/cache/input 四字段）再按 `input_tokens()` 决策；`apply` 的 no-op 打 debug；`apply_bounds` splice 打 WARNING（before/after/summarized_start/end/removed/summary_chars/step）；PTL retry 由 info 升 WARNING 并加 `dropped` 字段。测试 3 条（含 2 条 headline 的日志断言版）。
- `src/compact/micro.rs` — prune 实际生效时 WARNING（pruned / tool_messages / keep_recent / trigger）。
- `src/runtime.rs` — `maybe_compact_cross_turn` 触发行改走 `should_compact_usage`（#162 的实际回归位，run_core 内轮触发由 #330 的 last-call 语义另行覆盖，不触碰 run_core）。
- `tests/compact_contract.rs` — +4：`test_compact_contract_token_normalized`（headline）、
  `test_compact_contract_negative_control_unnormalized_trigger_bypass`（经
  `maybe_compact_cross_turn` 驱动的运行时级负控）、`test_compact_contract_degradation_warning`
  （runtime 级 headline：splice 后 transcript 形状 + no-op 不改写；字面日志文本钉在 lib 内测试，
  因 `logs_contain` 只捕获与本测试 crate 同名 target）、既有 11 条未回退。

## Tests added / results

- `cargo test -p recursive-agent`：3842 passed / 0 failed（含 compact_contract 14 条、lib 内 3 条新测试）。
- `cargo test --workspace`（--exclude recursive-agent 计数 + recursive-agent 全量）：全绿。
- `cargo clippy --all-targets --all-features -- -D warnings`：clean（4m10s）。
- `cargo fmt --all -- --check`：clean。

### 变异负控（全部手工验证 → 红）

1. runtime 触发行回退 `should_compact(bytes, usage.prompt_tokens)` →
   `test_compact_contract_negative_control_unnormalized_trigger_bypass` 红。
2. 删 `should_compact_usage` 的归一 WARNING → `test_compact_contract_token_normalized_warns` 红。
3. 删 `apply_bounds` 的 splice WARNING → `test_compact_contract_degradation_warning`（lib 版）红。
4. 删 micro.rs 的 prune WARNING → `test_compact_contract_microcompaction_warns` 红。

## Notes

- `should_compact(usize, u32)` 旧签名保留（run_core 内轮触发仍在用，goal 明令不碰 run_core）；
  新入口 `should_compact_usage` 是消费侧钉子，runtime 的跨轮触发已切换。
- WARNING 全部带 `target: "recursive::compact"`，字段结构化（before/after/summarized 区间等），
  满足 owner 红线「任何降级必须可观测」；no-op 与未触发保持 debug/静默。
- 快照 apply 后与 WIP 完全一致（5 文件 +466/-3），本 run 无新增源码改动，仅补验证与负控执行留痕。

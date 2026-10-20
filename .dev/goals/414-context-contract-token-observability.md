# Goal 414 — 上下文与压缩契约（2/2 之 2）：token 归一钉子 + 降级可观测

**Tracking**: recursive#197 后半（后）/ #186 收口的最后维度。前置 = Goal 411（7ad601b，`Compactor::apply` 单一路径）+ Goal 413（f20b31cb，canonical rebuild + rebuild markers 回归）均已落 origin/main。
**拆分注记**: 本 goal 与 413 原为同一 goal，413 首试 wall-clock 实证全量超容量而拆分；本 half 体量约为 413 的一半。

**Design principle check**:
- ✅ token 口径单源于 adapter（LLM 层归一一次做对），压缩触发/预算只消费归一后数值。
- ✅ 一切降级（摘要替换/截断/丢弃）WARNING 级可观测（AGENTS.md owner 红线），合法 no-op 保持 debug。
- ❌ Does NOT 改 `src/kernel.rs` / `run_core.rs::run_inner` / `.dev/**` / `docs/**` / `crates/recursive-tui/**` / `src/multi.rs` / `src/coordinator.rs`。
- ❌ Does NOT 重写 rebuild 模块语义（Goal 413 已收口，tests/compact_contract.rs 11 条回归在树——不回退）。

## Why

#162/#163 的修复（usage 归一含缓存 token）只在 adapter 层做了一次，但压缩触发与预算判断的**消费侧**没有「输入必须已归一」的钉子——#162 本身就是一次真实回退（跨轮压缩用整轮累计 prompt_tokens，回退了 20fcdbcb 的修复）。降级侧：摘要替换/截断/丢弃路径历史上静默（debug 级或不记），违反 owner 红线。

## Scope（do exactly this, no more）

1. **token 归一钉子**：在压缩触发与预算判断入口加断言/守卫——消费的 `TokenUsage` 必须来自 adapter 归一路径（含缓存 token 折算）；输入未归一时显式 WARNING 并按保守口径处理。
2. **降级可观测**：`src/compact/` 与 `src/runtime.rs` 压缩路径的一切摘要替换/截断/丢弃打 WARNING 到 `recursive::compact`（结构化字段：before/after 消息数、被摘要区间）；合法 no-op（`split <= prefix_len` 等）保持 debug。
3. **回归测试**：每条钉子/观测点一条测试（钉住 #162/#163 不再回退）；变异负控：删守卫必须红。

## Files NOT to touch

`src/run_core.rs`、`src/kernel.rs`、`.dev/**`、`docs/**`、`crates/recursive-tui/**`、`src/multi.rs`、`src/coordinator.rs`、rewind/session 恢复族实现（#189）。tests/compact_contract.rs 既有 11 条回归不回退。

## Tests

- headline：`test_compact_contract_token_normalized` / `test_compact_contract_degradation_warning`
- 负控：删归一断言 → 红；删 WARNING → 红。

## Acceptance

```bash
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo test compact_contract
```

## Notes for the agent

- 基线含 Goal 411（Compactor::apply）与 413（rebuild markers）——读它们再动手；**勿回退 tests/compact_contract.rs 任何既有断言**。
- 本 goal 体量小（413 拆分的余量），预算充足；墙钟 7200s。

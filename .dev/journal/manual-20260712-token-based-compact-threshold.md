# Manual edit: token-based-compact-threshold

**Date**: 2026-07-12
**Goal**: 修复 compaction 触发阈值计算 bug。原来仅使用字符数估算（假设 4 chars/token），对中文内容严重低估 token 消耗（中文实际约 2.57 tokens/char），导致上下文溢出前未能触发 compaction。改为优先使用 API 返回的实际 prompt_tokens 触发 compaction，字符估算作为降级方案。

**Root cause**: 某个 75 轮对话会话（全中文）累积了 77,683 字符，字符阈值未触发，但实际输入 token 已达 199,906，超过 z-ai/glm-5.2 的 200K 上下文窗口，API 调用失败，后续用户消息无响应。

**Files touched**:
- `providers.toml` — 新增 `z-ai/glm-5.2` 和 `z-ai/glm-5.2[1m]` 条目（200K / 1M 上下文窗口）
- `src/llm/pricing.rs` — 新增 `default_compact_threshold_tokens()` 函数，基于模型上下文窗口计算 token 触发阈值（保留 20K 给摘要，取 80% 有效窗口）
- `src/llm/mod.rs` — 导出 `default_compact_threshold_tokens`
- `src/compact.rs` — 为 `Compactor` 新增 `threshold_prompt_tokens: Option<u32>` 字段和 builder setter
- `src/runtime.rs` — `maybe_compact_cross_turn` 接收 `last_prompt_tokens: u32`，优先用 token 阈值判断，降级到字符估算
- `crates/recursive-cli/src/cli/builder.rs` — 在构建 `Compactor` 时同时设置 `threshold_prompt_tokens`

**Tests added**:
- `src/llm/pricing.rs::tests::compact_threshold_tokens_unknown_model_uses_fallback_128k`
- `src/llm/pricing.rs::tests::compact_threshold_tokens_glm5_2_uses_200k_window`
- `src/llm/pricing.rs::tests::compact_threshold_tokens_glm5_2_1m_uses_1m_window`
- `src/llm/pricing.rs::tests::compact_threshold_tokens_is_less_than_context_window`
- `src/compact.rs::tests::threshold_prompt_tokens_setter_works`
- 更新已有测试 `default_threshold_is_max` 和 `builder_methods_work` 断言新字段

**Notes**:
- API 端不返回模型的 context_window 容量，故使用 providers.toml 中已知的模型上下文窗口作为阈值依据
- 使用实际 `prompt_tokens`（来自上一轮 API 响应的 `usage` 字段）作为触发条件，比字符估算精准
- 字符估算降级路径保留，确保旧会话（无 token 数据）也能正常触发 compaction
- 所有质量门通过：`cargo test --workspace`（全绿）、`cargo clippy --all-targets --all-features -- -D warnings`（无警告）、`cargo fmt --all`

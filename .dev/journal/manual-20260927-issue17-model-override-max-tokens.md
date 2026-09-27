# manual-20260927-issue17-model-override-max-tokens

## Date
2026-09-27

## Goal
Issue #17: `recursive --model <X>` 覆盖 `config.model` 后不重新解析
`max_tokens`，切到 output 上限更低的模型时首个请求即 HTTP 400
（发送的是上一个模型的 cap，如 384000 > 128000）。

## Files touched
- `src/config.rs` — 抽出 `derive_max_tokens(preset, model, file)` 共享
  三级推导（preset ModelSpec → 文件 `agent.max_tokens` → `DEFAULT_MAX_TOKENS`），
  `from_env` 改为调用它；新增 `Config::resolve_max_tokens()`，在非 env
  来源全部折入后重跑同一推导，显式 `RECURSIVE_MAX_TOKENS` 仍最高优先
  且照常走 `parse_env` 校验。
- `crates/recursive-cli/src/main.rs` — CLI 覆盖项（--model 等）折入后
  统一调用 `config.resolve_max_tokens()`。
- `crates/recursive-tui/src/runtime_builder.rs` — `/model` 热切换同病：
  抽出 `config_for_preset_model()`，先设 `config.preset`（重推导靠它查
  ModelSpec）再 `resolve_max_tokens()`，`build_provider_for_model` 用它
  构建交换后的 provider。

## Tests added
- `src/config.rs`:
  - `resolve_max_tokens_rederives_after_model_override`（#17 主场景，
    providers.d two-cap preset：big-cap 384000 → small-cap 128000）
  - `resolve_max_tokens_lets_explicit_env_win_over_model_override`
  - `resolve_max_tokens_falls_back_to_file_tier_for_unknown_model`
  - `resolve_max_tokens_without_preset_is_default`
- `crates/recursive-tui/src/runtime_builder.rs`:
  - `model_hot_swap_rederives_max_tokens`（直接断言 `config_for_preset_model`，
    ChatProvider 不暴露 cap，config 是可观测缝）

## Notes
- 顶层 `--provider` 只改协议类型（provider_type），不改 active preset，
  所以它不影响 max_tokens 推导；issue 里「Option 1 覆盖 --provider」的
  前提在本仓不成立，但统一在覆盖项折入后重推导的写法对它是无害的。
- 影响面（手工 grep 分析；本会话无 gitnexus MCP）：production 里加载后
  改 `config.model` 的只有 CLI main.rs 与 TUI runtime_builder.rs 两处，
  均已覆盖；`multi.rs` 的 Config 字面量是 `#[cfg(test)]` fixture，
  `team.rs` 的 `with_model` 是成员描述符，与运行时 Config 无关。
- 真机冒烟：mock OpenAI 网关捕获请求体，`--model small-cap` 发送
  `max_tokens=128000`、默认模型发送 384000、显式 env 4096 照旧。

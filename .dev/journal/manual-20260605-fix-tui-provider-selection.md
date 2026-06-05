# Manual edit: fix-tui-provider-selection

**Date**: 2026-06-05
**Goal**: Fix TUI always using OpenAiProvider regardless of config.provider_type
**Files touched**: `src/tui/runtime_builder.rs`
**Tests added**: none (existing offline_mode test still passes)
**Notes**:
- Root cause: `build_runtime` and `build_runtime_with_skill_tx` both hardcoded
  `OpenAiProvider::new(...)`, ignoring `config.provider_type`. So even with
  `type = "anthropic"` in config.toml, TUI used OpenAI provider — meaning
  `AnthropicProvider`'s deferred tool / ToolSearch logic was never exercised.
- Fix: extracted `build_provider(config, api_key)` that mirrors the same
  `match provider_type { "anthropic" => ..., _ => ... }` logic in `cli/builder.rs`.
  Both `build_runtime` and `build_runtime_with_skill_tx` now call it.
- Also added missing `Duration` and `RetryPolicy` imports, and wired retry
  policy from config (was previously using defaults in TUI).

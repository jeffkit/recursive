# Manual edit: model context window moved to providers.toml

**Date**: 2026-06-03
**Goal**: Remove hardcoded context window lookup; maintain all model metadata in one config file.

**Files touched**:
- `providers.toml` — each model entry is now `{name, context_window}` inline table
- `src/providers.rs` — new `ModelSpec` struct; `ProviderPreset.models: Vec<ModelSpec>`
- `src/llm/mod.rs` — `context_window_tokens_for_model()` queries `all_presets()` directly
- `src/lib.rs` — re-export `ModelSpec`

**Tests added**: updated existing 3 tests to use real model names from providers.toml

**Notes**:
- To update a model's context window: edit `providers.toml` only, no Rust change needed
- providers.toml is embedded at compile time via `include_str!`, so it's still zero-cost at runtime

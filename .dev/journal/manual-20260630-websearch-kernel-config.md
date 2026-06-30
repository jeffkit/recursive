# Manual edit: web-search kernel config propagation

**Date**: 2026-06-30
**Goal**: Fix TUI not picking up `[search]` config from config.toml for WebSearch

**Problem**: TUI (`cargo tui`) calls `build_standard_tools_with_roots()` which
created `WebSearch::new()` with no config overrides — `load_config()` fell back
to env vars only, ignored config.toml. CLI worked because it manually called
`with_search_config()` with Config values. Root cause: config propagation was
at the frontend level (CLI only), not in the kernel.

**Fix**:
- `src/tools/registry.rs` — Added `web_search_provider`, `web_search_api_key`,
  `web_search_jina_key` parameters to `build_standard_tools_with_roots()`,
  passed them to `WebSearch::with_search_config()`. Added
  `#[allow(clippy::too_many_arguments)]`.
- `src/tools/registry.rs` — Updated `build_standard_tools()` wrapper to pass
  `None, None, None` (backward compatible).
- `crates/recursive-tui/src/runtime_builder.rs` — Both `build_runtime()` and
  `build_runtime_with_skill_tx()` now pass config values.

**Files touched**: `src/tools/registry.rs`, `crates/recursive-tui/src/runtime_builder.rs`
**Tests added**: none (existing tests pass)
**Quality gates**: `cargo test --workspace` (0 failed), `cargo clippy --all-targets --all-features -- -D warnings` (clean), `cargo fmt --all --check` (clean)

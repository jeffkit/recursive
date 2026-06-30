# Manual edit: config-to-toml

**Date**: 2026-06-16
**Goal**: Add config.toml support for env-only configuration items: web_search, stuck_detection, limits
**Files touched**:
- `src/config_file.rs` — Added `SearchSection`, `StuckSection`, `LimitsSection` to `FileConfig`
- `src/config.rs` — Added `web_search_provider`, `web_search_api_key`, `web_search_jina_key` fields to `Config`; added file config fallback for `stuck_window`, `stuck_error_rate`, `max_search_rounds`, `subagent_max_depth`, `max_concurrent_runs`, `goal_eval_transcript_tail`
- `src/tools/web_search.rs` — Added `provider_override`, `api_key_override`, `jina_key_override` fields; `with_search_config()` builder; `load_config(&self)` now checks struct fields before env vars; `search_jina_fallback()` checks `jina_key_override` first
- `crates/recursive-cli/src/cli/builder.rs` — Passes `config.web_search_*` to `WebSearch::with_search_config()`
- `tests/v050_integration.rs`, `tests/http.rs`, `tests/agui_e2e.rs`, `src/multi.rs`, `crates/recursive-cli/src/main.rs` — Added `web_search_*` fields to Config literals
**Tests added**: `parse_search_stuck_limits_sections`, `search_stuck_limits_are_optional` in config_file.rs
**Quality gates**: `cargo test --workspace` (1296 passed), `cargo clippy --all-targets --all-features -- -D warnings` (clean), `cargo fmt --all` (applied)
**Notes**:
- New config.toml sections: `[search]` (provider, api_key, jina_key), `[stuck]` (window, error_rate), `[limits]` (max_search_rounds, subagent_max_depth, max_concurrent_runs, goal_eval_transcript_tail)
- Priority chain unchanged: env var > file config > default
- WebSearch tool now accepts config overrides; env var fallback preserved

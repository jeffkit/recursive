# Manual edit: tui-context-usage-gauge

**Date**: 2026-07-21
**Goal**: Show a live context-window token usage gauge at the bottom-right of the TUI input box so the user can feel how full the context window is in real time.
**Files touched**:
- `crates/recursive-tui/src/cost.rs` — added `UsageStats::last_prompt_tokens` (set in `record_with_cache` to `max(input_tokens, cache_hit + cache_miss)` so it is correct for both Anthropic, where `input_tokens` excludes cached tokens, and OpenAI without cache reporting, where the cache sum is 0 and `input_tokens` already carries the full prompt). Added `detect_context_window()` helper mirroring `detect_model_name()`.
- `crates/recursive-tui/src/app/mod.rs` — added `App::context_window: u64` field + re-export of `detect_context_window`.
- `crates/recursive-tui/src/app/state.rs` — initialise `context_window` in `App::new()`.
- `crates/recursive-tui/src/ui/input.rs` — `render()` now splits the 1-row footer hint into a left hint column and a right-aligned `ctx <used>/<window> · <pct>%` gauge. Colour ramps green (<70%) → yellow (<90%) → red (>=90%). Falls back to hint-only on narrow terminals.
**Tests added**:
- `cost::tests::record_with_cache_sets_last_prompt_tokens_to_cache_sum_for_anthropic`
- `cost::tests::record_with_cache_falls_back_to_input_tokens_without_cache_report`
- `cost::tests::detect_context_window_returns_nonzero`
- `ui::input::tests::context_gauge_returns_none_when_window_unknown`
- `ui::input::tests::context_gauge_formats_used_over_window_with_pct`
- `ui::input::tests::context_gauge_color_ramps_with_usage`
- `ui::input::tests::render_draws_gauge_in_footer_when_room_available`
- `ui::input::tests::render_falls_back_to_hint_only_on_narrow_terminal`
**Notes**:
- The gauge reads `app.usage.last_prompt_tokens`, which is updated on every `UiEvent::Usage` (per LLM call), so within a multi-step tool-use turn it tracks the most recent prompt size — the best live proxy for current context usage. The window size is resolved once at startup from `Config::context_window_tokens()` (honours `context_window_override`).
- Pre-existing failures observed in `src/providers.rs` (`effective_presets_*`) are unrelated — confirmed they fail with my changes stashed. They come from in-flight modifications to `src/llm/openai.rs` / `src/providers.rs` that appeared in the working tree during this session (not made by me). Left untouched.
- `cargo fmt --all` reports pre-existing fmt debt in `src/providers.rs` (also not mine); my TUI files are fmt-clean.
- Quality gates for the touched crate: `cargo test -p recursive-tui` (683 passed), `cargo clippy -p recursive-tui --all-targets -- -D warnings` clean, `cargo fmt -p recursive-tui` clean.

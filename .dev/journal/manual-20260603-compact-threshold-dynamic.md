# Manual edit: dynamic compaction threshold

**Date**: 2026-06-03
**Goal**: Replace hardcoded / env-only compaction threshold with a model-aware
auto-compute strategy, mirroring fake-cc's `getAutoCompactThreshold`.

**Files touched**:
- `src/llm/mod.rs` — added `context_window_tokens_for_model()` and
  `default_compact_threshold_chars()` public functions with unit tests
- `src/lib.rs` — re-exported both new functions
- `src/cli/builder.rs` — replaced the `RECURSIVE_COMPACT_THRESHOLD`-only branch
  with a three-way policy: explicit env override / auto-compute / disabled (0)

**Tests added**:
- `llm::tests::context_window_known_models`
- `llm::tests::context_window_unknown_model_fallback`
- `llm::tests::default_compact_threshold_is_reasonable`

**Notes**:
- Strategy matches fake-cc: `(contextWindow - reservedForSummary) * 0.8 * 4 chars/token`
- Reserved tokens = `min(20_000, contextWindow / 4)` to avoid reserving more than
  25 % of small windows (e.g. gpt-3.5's 16 K)
- Unknown models fall back to 128 K (conservative minimum for current-gen frontier models)
- RECURSIVE_COMPACT_THRESHOLD=0 / off / false explicitly disables compaction

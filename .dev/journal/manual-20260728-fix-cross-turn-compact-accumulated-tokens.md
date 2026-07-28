# Manual edit: fix-cross-turn-compact-accumulated-tokens

**Date**: 2026-07-28
**Goal**: Fix cross-turn compaction using accumulated prompt_tokens (turn sum) instead of the last single-call value, causing premature compaction on multi-step turns (e.g., deepseek-v4-flash with 1M context triggering compaction at ~100K actual usage because 8 tool-use LLM calls sum to 800K > 784K threshold).

**Root cause**: `runtime.rs:316` passed `turn_outcome.usage.prompt_tokens` (accumulated sum across all LLM calls in the turn) to `maybe_compact_cross_turn`, which passes it to `Compactor::should_compact`. This inflated value triggered compaction prematurely.

**Fix**:
1. Added `last_prompt_tokens: u32` to `RunInnerOutcome` (populated from `RunCore.last_prompt_tokens`)
2. Added `last_prompt_tokens: u32` to `TurnOutcome` (threaded through from `RunInnerOutcome`)
3. Changed `runtime.rs:316` to use `turn_outcome.last_prompt_tokens` instead of `turn_outcome.usage.prompt_tokens`

**Files touched**:
- `src/run_core.rs` — `RunInnerOutcome` struct + `make_outcome` method
- `src/kernel.rs` — `TurnOutcome` struct + construction site + `turn_outcome_default_values` test
- `src/runtime.rs` — `maybe_compact_cross_turn` call site (line 316)
- `src/compact/mod.rs` — regression test `cross_turn_compact_uses_last_not_accumulated_prompt_tokens`

**Tests added**:
- `cross_turn_compact_uses_last_not_accumulated_prompt_tokens` — verifies that accumulated (800K) would trigger compaction while last_call (100K) correctly does not, documenting the fix.

**Notes**:
- kernel.rs line limit (1000): trimmed doc comment on new field to keep file at 998 lines.
- Quality gates: cargo test --workspace (all pass), clippy (0 warnings), fmt (clean).
- The intra-turn `maybe_compact` in `run_core.rs` already correctly uses `self.last_prompt_tokens` (single-call value) — only the cross-turn call site was wrong.

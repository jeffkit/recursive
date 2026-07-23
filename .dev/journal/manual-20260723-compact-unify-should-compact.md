# Manual edit: Unify compaction threshold decision into `Compactor::should_compact` (Goal 330)

**Date**: 2026-07-23
**Goal**: Extract one `should_compact` predicate on `Compactor` that both
intra-turn (`run_core.rs::maybe_compact`) and cross-turn
(`runtime.rs::maybe_compact_cross_turn`) call, fixing the bug where the
intra-turn path ignored `threshold_prompt_tokens`.

**Files touched**:
- `src/compact/mod.rs` — added `Compactor::should_compact(&self, estimate_chars: usize, last_prompt_tokens: u32) -> bool` method (verbatim logic from `runtime.rs:378` inline match). Added 4 unit tests.
- `src/run_core.rs` — added `last_prompt_tokens: u32` field to `RunCore` struct, initialized to `0` in `make_test_core` and `make_run_core_for_inner`. Updated `dispatch_llm_step` to store `self.last_prompt_tokens = u.prompt_tokens` when `completion.usage` is available. Changed `maybe_compact` char-only guard to `compactor.should_compact(chars, self.last_prompt_tokens)`. Added `maybe_compact_uses_token_threshold_intra_turn` regression test.
- `src/runtime.rs` — replaced inline threshold `match` in `maybe_compact_cross_turn` with `compactor.should_compact(chars, last_prompt_tokens)`.
- `src/kernel.rs` — added `last_prompt_tokens: 0` to the `RunCore` construction site.

**Tests added**:
- `compact::tests::should_compact_uses_token_threshold_when_available`
- `compact::tests::should_compact_falls_back_to_chars_when_tokens_zero`
- `compact::tests::should_compact_falls_back_to_chars_when_threshold_none`
- `compact::tests::should_compact_zero_token_threshold_fires_on_zero`
- `run_core::tests::maybe_compact_uses_token_threshold_intra_turn`

**Quality gates**: `cargo fmt --all` ✓, `cargo clippy --all-targets --all-features -- -D warnings` ✓, `cargo test --workspace` ✓ (2043 passed).

**Notes**:
- This is a pure refactor + bug fix — the threshold logic is byte-for-byte equivalent
  to the existing `runtime.rs:378` match. No threshold values changed.
- `last_prompt_tokens` is `u32`, not `Option<u32>` — `0` means "no reading yet"
  and the predicate already handles `actual > 0`. No `Option` needed.
- The intra-turn `maybe_compact` runs at the START of each step (before the LLM
  call), so `self.last_prompt_tokens` holds the PREVIOUS step's reading on step
  N≥2, and `0` on step 1. That is correct: we compact based on the most recent
  known context size.
- Did NOT modify `src/llm/`, `src/kernel.rs` (other than the field init),
  `src/agent/types.rs`, `src/message.rs`, `crates/`, `src/http/`, or any tool file.
- Did NOT add a circuit breaker (that is goal 331).

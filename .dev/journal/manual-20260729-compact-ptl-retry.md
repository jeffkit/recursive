# Manual edit: compact-ptl-retry

**Date**: 2026-07-29
**Goal**: 337 — Self-rescue when the compaction request itself hits prompt-too-long

## Summary

Added a retry loop inside `Compactor::compact` that catches
context-window-exceeded errors from the summarisation call and drops
oldest message groups (by User/System boundaries) before retrying, up
to 3 times, with a fallback drop fraction when the error token gap is
unparseable.

## Files touched

- **`src/error.rs`** — Added `pub fn is_context_window_exceeded(err: &Error) -> bool`
  (moved private function from `runtime.rs` to shared location so `compact/mod.rs`
  can call it without a circular dependency).
- **`src/runtime.rs`** — Replaced private `fn is_context_window_exceeded` with
  `use crate::error::is_context_window_exceeded;`.
- **`src/compact/retry.rs`** — New module with:
  - `group_by_user_boundary(messages)` — groups messages at User/System boundaries
  - `truncate_head_for_retry(messages, target_chars)` — drops oldest groups until
    remaining chars ≤ target (or by fallback 1/5 fraction), retreats past orphaned
    Tool/Asst+tc messages, optionally prepends user marker
  - `estimate_target_from_error(err)` — best-effort token-gap parsing from LLM errors
  - 10 unit tests covering all branches
- **`src/compact/mod.rs`** — Added `pub mod retry;` + `pub use ...`; extracted
  `render_for_summarize` and `summarize` private methods; added retry loop
  inside `compact()` method (3 new integration tests).

**Did NOT touch**: `src/run_core.rs`, `src/kernel.rs`, `src/llm/`, tool files,
or any other module.

## Quality gates

- `cargo test --workspace`: 2128 passed, 0 failed
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all`: clean
- `tests/invariants/tool_call_pairing.rs`: all 35 invariants pass

## Notes

- The retry loop covers both intra-turn (`run_core::maybe_compact`) and
  cross-turn (`runtime::maybe_compact_cross_turn`) callers automatically
  since both call `Compactor::compact`.
- `is_context_window_exceeded` is now in `error.rs` (shared), not duplicated.
- `estimate_target_from_error` is best-effort; returns `None` on parse
  failure (→ fallback drop fraction of 1/5 of groups).
- The three new `compact::tests` tests verify: PTL retry succeeds on 2nd
  attempt, gives up after `MAX_PTL_RETRIES+1` calls, and non-PTL error
  propagates immediately (no retry).

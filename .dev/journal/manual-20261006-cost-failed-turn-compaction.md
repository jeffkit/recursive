# Issue #115 — failed-turn and compaction token accounting

Date: 2026-10-06
Goal: #115 fix(cost) — stop dropping token usage on turn failure and on
compaction, and expose a `tokens_wasted_on_failure_total` counter.

## Problem

Two independent leaks made quota/budget calibration systematically optimistic:

1. `RunCore::run_inner`'s generic `Err(e) => return Err(e)` dropped the turn's
   accumulated `total_usage`, and no consumer (`handlers.rs` failure branches,
   CLI/AG-UI `record_usage`) accepted usage on the failure path — so a
   "provider 500 after 50 steps" turn was billed as 0.
2. `Compactor` took only `completion.content` from the summarisation call; the
   `usage` (and `complete_structured`'s) was discarded. Intra-turn compaction,
   cross-turn compaction and the overflow-retry compaction all recorded 0.

## Files touched

- `src/run_core.rs` — new `compaction_usage` field (folded into `total_usage`
  in `make_outcome`); new `failure_usage` sink + `fail_step` helper that
  publishes the partial usage before propagating the error.
- `src/kernel.rs` — `TurnContext::failure_usage` + `FailureUsage` alias; wired
  into the `RunCore` literal.
- `src/runtime.rs` / `src/runtime/builder.rs` — `last_failed_usage` /
  `pending_compact_usage` fields, `last_failed_usage()` accessor;
  `maybe_compact_cross_turn` and `compact_on_overflow` now return their
  summarisation usage, folded into the turn outcome (or the failed account).
- `src/compact/mod.rs` — `CompactionOutcome { removed, summary_chars, usage }`;
  `compact`/`summarize`/`try_structured_compact`/`apply_to_transcript` thread
  the `TokenUsage` through (accumulated across PTL retries).
- `src/llm/chat.rs` — `StructuredCompletion { value, usage }`.
- `src/llm/mod.rs`, `src/llm/openai.rs`, `src/llm/mock.rs` — `complete_structured`
  returns `StructuredCompletion` so the structured compaction path bills too.
- `src/http/mod.rs`, `src/http/handlers.rs`, `src/http/agui.rs` —
  `tokens_wasted_on_failure_total` counter (exposed on `/metrics`),
  `record_run_failed(metrics, usage)`.
- `crates/recursive-cli/src/main.rs`, `crates/recursive-cli/src/cli/resume.rs` —
  bill `last_failed_usage` into the control session and the cost tracker; write
  a cost record on the error path.

## Tests added

- `run_core::tests::run_inner_publishes_partial_usage_when_the_turn_errors`
- `run_core::tests::make_outcome_folds_intra_turn_compaction_usage_into_total`
- `runtime::tests::failed_turn_exposes_last_failed_usage`
- `compact::tests::apply_to_transcript_splices_and_returns_counts` now asserts
  `outcome.usage`; `compact::tests::compact_retries_on_ptl_then_succeeds`
  asserts the reported usage.
- `http::handlers::tests::record_run_metrics_track_llm_streak` asserts the new
  counter.
- Existing `compact_on_overflow` tests updated for `Result<Option<TokenUsage>>`.

## Notes

- No new dependencies.
- `RunCore::run_inner` stays at 149 body lines (invariant #1 limit 150): the
  failure path was extracted into `fail_step` instead of growing the loop.
- `CompactionBoundary`'s documented g336 cache-telemetry semantics (previous
  turn's cache counts) are unchanged; only the overflow path now emits the
  summarisation call's own cache counts instead of a hardcoded 0.
- Runtime visibility of the new counter (Langfuse) is issue #124 territory.

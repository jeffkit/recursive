# Loop retry vs per-step retry — `run_loop` retry budget

## Date
2026-10-05

## Goal
`tests/run_loop_wakeup_persist.rs::run_loop_gives_up_after_the_retry_budget`
failed: with `LoopRetryPolicy::new(2, …)` and a provider whose first three
calls fail (HTTP 503), `run_loop` returned `Ok` after four LLM calls instead of
`Err` after three.

Root cause: issue #100's per-step retry (`sandbox`-free ReAct step retry,
`RetryPolicy::for_step_loop`, 3 retries) is still active inside a loop turn, so
the two retry budgets nest. The three injected errors were consumed by the
step retry within the *first* turn, and its fourth attempt hit the scripted
completion — the turn "recovered", the loop-level budget never applied, and the
worst case became `step_retry × loop_retry` LLM calls / backoff waits.

## Files touched
- `src/runtime.rs` — `run_loop` now swaps the kernel's `step_retry` for a
  no-retry policy (`max_retries: 0`) for the duration of the loop and restores
  it when the loop ends; the previous body moved into the private
  `run_loop_inner` so the restore happens on every exit path (including the
  failed-turn `Err`). In loop mode the loop-level retry owns transient
  failures: it re-drives the turn from the failed step (the failed attempt's
  committed messages are folded back into the transcript), so the inner budget
  only multiplied the worst case.

## Tests added
None — behaviour pinned by the existing
`tests/run_loop_wakeup_persist.rs` (all 6 tests pass; the previously failing one
now observes `initial attempt + 2 retries, then give up` = 3 LLM calls).
`src/runtime/tests.rs::llm_retry_recovers_and_emits_event` still covers the
per-step retry for non-loop turns (`AgentRuntime::run`).

## Notes
- `cargo test --workspace --no-fail-fast`: 4052 passed, 0 failed.
- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --all`: no diff.

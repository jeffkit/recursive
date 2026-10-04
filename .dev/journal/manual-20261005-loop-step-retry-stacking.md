# Manual edit: loop / step retry no longer stack

**Date**: 2026-10-05
**Goal**: `cargo test --workspace` was red on
`tests/run_loop_wakeup_persist.rs::run_loop_gives_up_after_the_retry_budget`.

## Root cause

Two retry layers covered the same transient provider failure:

- step-level — `RunCore::dispatch_llm_step_with_retry` (issue #100, default
  `RetryPolicy::for_step_loop` = 3 retries);
- turn-level — `AgentRuntime::run_turn_with_retry` (issue #99,
  `LoopRetryPolicy`).

The budget test injects 3 errors into `MockProvider` and expects the
`loop_retry` budget (`fast_retry` = 2 retries) to be exhausted after
`1 + 2 = 3` calls. Instead the *step* layer ate all 3 errors first and the
4th call was served the scripted completion, so the turn succeeded and
`run_loop` returned `Ok` (4 calls) — the outer budget never applied.

## Change

`src/runtime.rs` — `run_loop` now suppresses the redundant inner layer for
the loop's lifetime (`self.kernel.step_retry.max_retries = 0`, restored on
return); the body moved to a private `run_loop_turns`. Loop mode owns retry
because `run_turn_with_retry` re-drives a failed turn *from the step it
stopped at* (issue #99 folds the attempt's committed messages back into the
transcript), so the outer layer already never re-runs a tool. The provider's
own per-request `RetryPolicy` is untouched.

## Verification

- `cargo test -p recursive-agent --test run_loop_wakeup_persist` — 6 passed.
- `cargo test --workspace --no-fail-fast` — 0 failed.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean.
- `cargo fmt --all` — clean.

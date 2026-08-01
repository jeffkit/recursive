# Manual edit: goal-358 — Test-cover `src/runtime/` subdir + `run_core.rs` production-size guard

**Date**: 2026-08-02
**Goal**: Close two test-coverage/invariant gaps: (C1) `src/runtime/builder.rs` and
`src/runtime/checkpoint.rs` had ZERO tests after being extracted out of the tested
`runtime.rs`; (C3) `run_core.rs` production LOC had no size gate (only the `run_inner`
function body was guarded). Test-only changes; no production logic, kernel, tools, or
providers touched.

## Files touched

- `src/runtime/builder.rs` — NEW `#[cfg(test)] mod tests` (4 tests):
  - `build_with_minimum_config_succeeds` — pins happy-path defaults (max_steps 0 =
    unlimited, no compactor/microcompactor, streaming off, empty transcript, checkpoints
    disabled, goal_eval_transcript_tail 12).
  - `build_without_llm_errors` — asserts the REAL failure mode: `AgentKernelBuilder::build`
    returns `Err(Error::Config { message: "llm provider is required" })`, which propagates
    through `AgentRuntimeBuilder::build()` (verified in `src/kernel.rs:533-536` before
    writing the test; it does not panic).
  - `builder_setters_round_trip` — max_steps(7), max_transcript_chars(5000),
    compactor(...), streaming(true), stuck_window(4), seed_transcript(msgs); asserted via
    `rt.kernel.max_steps` / `rt.kernel.max_transcript_chars` / `rt.kernel.stuck_window`
    (kernel fields are `pub(crate)`) and `rt.compactor` / `rt.streaming` / `rt.transcript`
    (private AgentRuntime fields reachable from the child-module tests).
  - `file_reinjector_and_skill_reinjector_wire_through` — pins Goal 334/335 wiring:
    both reinjectors land on the built runtime, and the Goal-340 plan/todo reinjector is
    always present.
- `src/runtime/checkpoint.rs` — NEW `#[cfg(test)] mod tests` (3 tests):
  - `turn_index_starts_at_zero_and_increments` — `AtomicUsize` starts at 0, `fetch_add`
    reads back correctly.
  - `disabled_state_has_no_checkpoint_subsystem` — `disabled()` sets every field to None
    and `enabled()` is false (guards accidental field additions / default changes).
  - `touched_files_lifecycle` — `disabled()` has `touched_files: None`; when attached,
    the `TouchedFiles` add/read-sorted/clear cycle works (pins the field wiring).
- `tests/invariants/loop_size_orthogonality.rs` — NEW `run_core_production_stays_small`
  test + `production_line_count` helper, placed directly after
  `run_inner_function_body_stays_small` so the two `run_core.rs` guards sit together.

## Design decisions / traps encountered

- **`build()` failure mode**: `AgentRuntimeBuilder::build()` delegates to
  `AgentKernelBuilder::build()` which returns `Err(Error::Config)` when `llm` is missing —
  no panic, no silent broken runtime. Test asserts `matches!(err, Error::Config { .. })`.
- **Field access from co-located tests**: `AgentRuntime` fields (private in `crate::runtime`)
  are visible from child module `crate::runtime::builder::tests`; `AgentKernel` fields are
  `pub(crate)`; so setters could be asserted directly (no behavioural indirection needed
  except transcript, which is observable via `rt.transcript`).
- **`Ordering` import**: `checkpoint.rs` top-level only imports `AtomicUsize`, so the test
  module adds `use std::sync::atomic::Ordering;`.
- **No `ShadowRepo` construction in tests** — `ShadowRepo::open` canonicalizes paths and
  shells out to git (heavy); `disabled_state_has_no_checkpoint_subsystem` covers the
  `enabled()` logic without it.
- **Cap = 1500** exactly as specified: current production portion is 1420 lines (line of
  `#[cfg(test)]` in `run_core.rs`; verified `grep -n` = 1420), giving ~80 lines of
  headroom. Guard is real: +81 production lines trips it.
- Used `.unwrap()`/`.expect()` in test code only — exempted by `cfg_attr(test, allow)`
  in `src/lib.rs`; clippy clean confirms.

## Verification

- `cargo test --lib runtime::builder::tests` → 4 passed.
- `cargo test --lib runtime::checkpoint::tests` → 3 passed.
- `cargo test --test invariants run_core_production_stays_small` → 1 passed.
- `cargo test --workspace` → all green, 0 failures (lib 2190 passed; all other binaries
  clean).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- `cargo fmt --all` → clean.
- `git diff --stat`: 3 files, +171/-0. No production code modified.

## Notes

- The `kernel.rs ≤ 1000` cap (currently 998) is untouched, per goal scope — this goal only
  adds the missing `run_core.rs` guard.
- Invariant #1 is unchanged; this adds the MEASUREMENT that makes its existing intent
  enforceable on `run_core.rs`.

## E2E gate fix round (2026-08-02, flow round 1/3)

The flow's `e2e-gate.sh` invocation failed with **exit 5 (`argus-init` → `SESSION_EXISTS`)**.
Root cause was **environmental, not source**: the flow's original gate run was interrupted
mid-flight (after `argus-init` succeeded, before `argus-clean` ran), which stranded the
argusai MCP server's per-project session state (`e2e/.argusai/history.db`) marked
"initialized", plus a stale `aimock` container. The next gate invocation re-initialized the
same project and was rejected. None of the goal's changes are involved — the smoke suite
replays the built binary against a mock LLM, and the new code is `#[cfg(test)]`-only.

Remedy applied: `rm -rf e2e/.argusai && docker rm -f aimock`, then re-ran the gate.
Verified `sh .dev/scripts/e2e-gate.sh` → RC=0 "smoke PASSED ✓" **twice consecutively with
no manual cleanup between runs**, proving a completed run leaves the state clean (argus-clean
resets `history.db`, container + aimock removed). Documented as failure mode #4 in
`AGENTS.md` (interrupted `e2e-gate.sh` run strands argusai session state → `SESSION_EXISTS`)
so future runs know the remedy. No product/source changes were needed for this round.

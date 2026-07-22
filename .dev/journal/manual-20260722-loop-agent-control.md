# Manual edit: loop-supervise lifecycle (agent-controlled stop + natural-language `/loop`)

**Date**: 2026-07-22
**Goal**: Make the event-driven loop's lifecycle transparent to the user: the
agent can stop the loop itself (so the user can say "停" in natural language
instead of typing `/loop stop`), and `/loop <prompt>` defaults to start so the
UX is natural-language first.
**Files touched**:
- `src/tools/run_background.rs` — added `LoopControl` enum + `loop_control`
  field + `set_loop_control` / `take_loop_control` on `BackgroundJobManager`
  (reuses the already-threaded shared manager, so no new slot threading).
- `src/tools/stop_loop.rs` (new) — `stop_loop` deferred tool; writes
  `LoopControl::Stop` onto the shared bg manager.
- `src/tools/mod.rs` — export `stop_loop` + `LoopControl`.
- `src/tools/registry.rs` — register `StopLoop` after `WatchFile`.
- `crates/recursive-tui/src/backend.rs` — `worker_loop` drains
  `bg_manager.take_loop_control()` before consulting the arbiter; on `Stop` it
  emits `LoopStopped` and clears `loop_state` (loop exits after the current
  turn). Added `loop_control_stop_drains_via_shared_bg_manager` contract test.
- `crates/recursive-tui/src/commands.rs` — `/loop <prompt>` default: the `_`
  arm now treats the whole line as a natural-language goal (unlimited turns,
  no `max N` parsing — so a goal containing "max" is kept verbatim). Updated
  the `/loop` command summary/usage. Replaced the old
  `cmd_loop_unknown_subcommand_errors` test with
  `cmd_loop_default_treats_args_as_natural_language_goal` and added an
  additive `cmd_loop_default_preserves_goal_with_trailing_max_word` guard.
- `crates/recursive-tui/src/supervise_sop.md` — document `stop_loop` (tool
  list + step 6): call it when the supervised command is done or the user
  asks to stop in natural language.
**Tests added**:
- `tools::stop_loop::tests::*` (3): set Stop, consumed-once, deferred.
- `backend::tests::loop_control_stop_drains_via_shared_bg_manager`.
- `commands::tests::cmd_loop_default_treats_args_as_natural_language_goal`,
  `cmd_loop_default_preserves_goal_with_trailing_max_word`.
**Notes**:
- Wiring: production `build_runtime` already passes the same `bg_manager`
  into `build_standard_tools_with_roots` AND stores it on `TuiRuntime`, so
  the agent's `stop_loop` and the arbiter share one manager. The test-only
  `Backend::spawn_with_runtime` builds its own bg manager (not shared with
  the injected `rt`'s tools), so a full stop_loop end-to-end test would need
  a shared-manager spawn path; instead the shared-state contract is unit-
  tested directly (set → take → None), and the tool→set path is covered in
  `stop_loop.rs`. The `worker_loop` drain itself is 4 lines of glue.
- `tui-test-presence` initially FAILED because the gate's regex matches only
  added `#[test]`/`#[cfg(test)]`/`mod tests` lines: my `commands.rs` test
  reused a context `#[test]` (it replaced a deleted test) and my `backend.rs`
  test uses `#[tokio::test]` (not matched). Added an additive `#[test]`
  (`cmd_loop_default_preserves_goal_with_trailing_max_word`) to satisfy the
  gate honestly. (Pre-existing gate gap: `#[tokio::test]` isn't recognised —
  noted here, not fixed, since `.dev/` is meta-tooling.)
- `tui-mutants` (advisory) to be run scoped to `backend.rs`+`commands.rs`
  after commit; survivors landing outside the diff hunks are pre-existing
  debt per CLAUDE.md, survivors inside the hunks will be fixed.

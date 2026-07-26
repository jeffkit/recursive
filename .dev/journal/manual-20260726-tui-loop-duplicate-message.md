# Manual edit: tui-loop-duplicate-message + input.rs cleanup

**Date**: 2026-07-26
**Branch**: `tui-loop-and-input-cleanup` (off `main` @ `88aac54`)
**Goal**: (1) Fix the duplicate "Loop started/stopped" messages shown when
starting/stopping an event-driven `/loop`. (2) Finish `ui/input.rs` — format
it, remove dead code left over from the line-wrap feature, and add direct
tests for the live cursor-positioning function.
**Files touched**:
- `crates/recursive-tui/src/commands.rs` — `/loop` duplicate-message fix.
- `crates/recursive-tui/src/ui/input.rs` — dead-code removal + wrapped-cursor tests.

## 1. `/loop` duplicate-message fix (`commands.rs`)

### Root cause
`/loop <goal>`, `/loop start <goal>`, and `/loop stop` each surfaced TWO
identical system lines on Enter:

1. `cmd_loop` pushed a `System` block synchronously the instant the command
   handler ran — an optimistic "you pressed the button" hint.
2. The backend's `StartLoop` / `StopLoop` handlers then sent
   `UiEvent::LoopStarted` / `UiEvent::LoopStopped`, and `event_loop.rs`
   pushed a second `System` block with the same text.

So every successful start/stop produced two near-identical lines.

### Fix
Dropped the synchronous `push_system` calls from the `start`, default, and
`stop` arms of `cmd_loop`. The backend events are the authoritative source:

- `LoopStarted` fires only after the loop-state mutex checks pass, so a
  *rejected* start surfaces an `Error` instead of a false "started" line.
- `LoopStopped` also covers the max-turns cap and the agent-requested
  `stop_loop` tool — driving the message from the event keeps `/loop stop`,
  cap-exhaustion, and agent stop all consistent (exactly one line each).

The `trigger` arm keeps its synchronous push (the backend enqueues triggers
silently, so that push is the only visible feedback). Status / usage-error
pushes unchanged.

### Tests
Three regression tests pin the no-duplication behaviour end-to-end (invoke
the command, pump the backend event, assert exactly one line):
`cmd_loop_{start,default,stop}_does_not_duplicate_*`. Plus the existing
stop test was updated to pump `LoopStopped` before asserting.

## 2. `ui/input.rs` cleanup

The line-wrap feature (`wrap_line_by_width`, `cursor_visual_position_wrapped`,
render integration) shipped earlier but left the tree half-finished:

- **Formatting**: the committed file was not rustfmt-clean (a `target_col`
  assignment + a missing trailing comma). Reformatted.
- **Dead code**: the pre-wrap cursor helper `cursor_visual_position`
  (non-wrapped) and the `visible_rows` helper (marked `#[allow(dead_code)]`)
  were fully superseded by the `_wrapped` variants and had zero production
  callers — only their own tests referenced them. Removed both functions and
  their tests.
- **Missing coverage**: `cursor_visual_position_wrapped` — the live
  cursor-positioning function used by `render` — had **no direct tests**.
  Added seven:
  - `empty_buffer`, `short_line_does_not_wrap`, `long_line_rows_and_cols`
    (mid-wrap / boundary / end cursor),
  - `multiline_counts_preceding_wraps`,
  - `sums_multiple_preceding_wraps` (catches the `wrapped_lines_before = …`
    assign-vs-compound-add mutant that a single preceding line can't),
  - `double_width_chars`, `mixed_ascii_and_cjk` (the byte-offset vs
    display-width distinction at wrap boundaries).

### Mutation testing
Attempted `cargo-mutants` on the file in copy mode; aborted because copy mode
cold-builds all deps separately per worker (~3× setup, no mutant ran in ~20
min) and the in-place path refuses uncommitted files. Did a manual
mutant-style review of `wrap_line_by_width` and `cursor_visual_position_wrapped`
instead: every plausible mutant (`==0`/`is_empty` flips, `>`↔`>=`, width
tracking, push conditions, byte-vs-width column) is caught by the tests
above. The one real gap — `wrapped_lines_before += …` mutated to `=` — is
closed by `cursor_visual_position_wrapped_sums_multiple_preceding_wraps`.

## 3. `src/run_core.rs` — NOT touched on this branch

An earlier version of this work also fixed `make_core_with_llm_and_events`
(five missing `RunCore` fields). That fix is now **redundant**: the
autonomous self-improve loop landed Goal 331 (compaction circuit breaker)
on `main`, whose commit added those same fields plus a sixth
(`consecutive_compact_failures`). So this branch carries no `run_core.rs`
change.

## Parallel-work note

While this work was in progress, the self-improve loop landed Goal 331 and
Goal 332 on `main` (compaction circuit breaker + microcompactor). My
`commands.rs` / `input.rs` changes were restored from `stash@{0}` (the loop's
branch switch had stashed them) via a surgical `git checkout stash@{0} --`
of just those two paths, avoiding a conflict on the now-superseded
`run_core.rs` hunk. Goal 332 also independently reformatted `input.rs` (same
three spots I had formatted), so the extracted version is a strict superset
of its change — no regression.

## Verification

- `cargo test -p recursive-tui --lib` → 764 passed, 0 failed.
- `cargo clippy -p recursive-tui` → no warnings originating in
  `crates/recursive-tui/`.
- `cargo fmt` on both touched files → clean.
- `.dev/scripts/tui-test-presence.sh` → PASS.

## Out of scope (pre-existing, not introduced here)

`cargo clippy --workspace -- -D warnings` is currently red on a **single
warning in `recursive-agent`** ("loop variable `i` is only used to index
`tool_indices`") introduced by Goal 331/332's compaction code. Left
untouched: that code is owned by the still-active self-improve loop (an
active `selfimprove-*` worktree is present), so editing it risks a
parallel-work conflict. The loop should clear its own lint.

# Goal 381 — Tests for wall-clock deadline and run_event_loop driver paths (tests-only)

**Roadmap**: Kernel / test coverage — the Goal-345 wall-clock finish path and the
loop-mode drivers (`run_loop`/`run_event_loop`) have ZERO tests

**Design principle check**:
- Implemented as: tests only — no production code changes. `#[cfg(test)]` additions in
  `src/run_core.rs` and `src/runtime.rs` (or `tests/`), driving existing public paths.
- ❌ Tests-only → `gate.e2e` diff-scope short-circuit skips the docker build; this goal
  should finish in minutes, not 25+.
- ❌ Does NOT touch the agent kernel or run loop (only exercises it).

## Why (verified 2026-08-03 by reading the code)

1. **Wall-clock deadline is dead code in the suite.** `run_core.rs:735` `if
   self.wall_timeout_secs == 0 { return Ok(false) }` guards the deadline check, and every
   test helper hardcodes `wall_timeout_secs: 0` (`tests/v050_integration.rs:65`,
   `tests/agui_e2e.rs:104`, `tests/http_common/mod.rs:69`,
   `tests/agent_team_integration.rs:81,314`). No test anywhere drives
   `wall_timeout_secs > 0`, so the `FinishReason::WallClockExceeded` branch and its
   step-accounting (`:730-747`) never execute. A regression there would pass CI.

2. **Loop-mode drivers have zero test callers.** `run_event_loop` is defined at
   `src/runtime.rs:1336` and only invoked by `crates/recursive-cli/src/main.rs:1975`.
   The wakeup-slot continuation (`wakeup_slot.lock()...take()`), background-job
   completion injection (`mgr.take_completed()`, `:1362`), the wakeup-beats-bg-job
   priority, and the "nothing to do → break" exits are all unverified. `run_loop` (the
   goal-driven variant) has the same gap — its `ExternallyCleared` break paths
   (`:1175-1299`) are also untested (only `run_goal_loop_stops_when_achieved` and
   `run_goal_loop_stops_at_max_turns` exist).

## Scope (do exactly this, no more)

### 1. Wall-clock deadline test (in `src/run_core.rs` test module)

Add a test that drives `run_inner` (or the test harness's run helper) with
`wall_timeout_secs: 1` and a goal/model script that would otherwise run longer (e.g. an
agent that keeps making tool calls), asserting:
- the run finishes with `FinishReason::WallClockExceeded`,
- the step count reflects the deadline accounting (whatever `:730-747` computes — pin
  the actual behavior, e.g. `steps == 1` after one tool call, or whatever the code does),
- the emitted turn event is `TurnFinished` with the wall-clock reason.

Mirror the existing test harness in `run_core.rs` (`run_inner` test helpers at
`:1649,1818,2700`-ish set `wall_timeout_secs: 0` — the new test sets 1).

### 2. `run_event_loop` driver tests (in `src/runtime.rs` test module)

Drive `run_event_loop` with a constructed `WakeupSlot` + `BackgroundJobManager`:
- **Wakeup continues**: arm a `WakeupSlot` that immediately has a pending wake → the
  loop wakes, runs a turn (or at least does not break), continues.
- **BG completion continues**: complete a background job (register in the manager +
  notify) → the loop observes it (does not break), handles the completion.
- **Nothing pending breaks**: with no wakeup armed, no bg job, and no other work → the
  loop breaks out with the expected "nothing to do" outcome.
- **Wakeup beats bg job** (priority): both armed → wakeup wins (pin the existing order).
Keep the harness minimal — if `run_event_loop` needs a full runtime, construct the
lightest one the existing tests already use.

### 3. (Optional, only if cheap) `run_goal_loop` external-clear break

A test that calls `clear_goal()` mid-loop (spawn a task that clears after a short
delay) and asserts the loop breaks without a duplicate `GoalCleared` — mirroring the
existing `run_goal_loop_stops_*` tests. Skip if the harness makes this expensive; the
two items above are the mandatory scope.

## Files NOT to touch

- Production code: `src/run_core.rs`, `src/runtime.rs` logic — tests only. If a test
  reveals a bug, DO NOT fix it in this goal: report it in the journal as a follow-up
  finding (and see the cycle's Phase 5 — a new finding may become a future goal).
- `.dev/flows/`, `.dev/scripts/`, `.flowcast/`.

## Acceptance

- `cargo build --workspace` green.
- `cargo test --workspace` green (including the new tests).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- `cargo fmt --all` clean.
- Headline tests by name:
  `cargo test --manifest-path Cargo.toml wall_clock` and
  `cargo test --manifest-path Cargo.toml run_event_loop` — new tests green.
- Grep: `rg "wall_timeout_secs: [1-9]" src/ tests/` — at least one non-zero driver in a
  test.
- Grep: `rg "run_event_loop" src/runtime.rs` — at least one test call site in `#[cfg(test)]`.

## Notes for the agent (traps)

- **Tests-only: no production edits.** If `run_inner` or `run_event_loop` misbehave
  under the new tests, write the finding into the journal — do not "fix" production code
  here. (The cycle supervisor collects such findings for a follow-up goal.)
- **`wall_timeout_secs` semantics**: read `run_core.rs:193` (doc) + `:730-747` before
  writing the test — the deadline is checked between steps; a 1-second budget with a
  fast tool-call loop may need the test to inject a small sleep or several calls to
  guarantee the deadline fires. Pin the actual accounting (assert the exact steps value
  the code produces, don't guess).
- **`run_event_loop` harness**: find how `main.rs:1975` constructs the runtime and
  mirror the minimal construction. `WakeupSlot` and `BackgroundJobManager` are both in
  `src/runtime.rs`/`src/tools/run_background.rs` — check existing tests
  (`take_completed_returns_finished_job`) for construction patterns.
- **cargo-fmt + clippy are enforced gates** — run both before finishing.
- **Journal**: write `.dev/journal/manual-<date>-goal381-deadline-loop-tests.md`.

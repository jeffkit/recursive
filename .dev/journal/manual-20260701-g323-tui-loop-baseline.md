# Manual edit: g323-tui-loop-baseline

**Date**: 2026-07-01
**Goal**: Land the g323 TUI event-driven loop driver (LoopArbiter + background-job
completion notify), kill all mutants on the touched TUI files, establish the
first whole-crate recursive-tui mutation baseline, and harden the mutation
tooling against the contamination accident that rolled back the prior run.

**Files touched**:
- `src/tools/run_background.rs` — added `completed_notify: Arc<Notify>` to
  `BackgroundJobManager`, `completed_notify()` accessor, terminal-state
  notification in `update()`. Fixed: `notify_waiters()` → `notify_one()` so a
  job completing while the arbiter is mid-turn isn't lost (permit stored).
  Removed 2 tests that deadlocked on `std::sync::Barrier` under the
  current-thread tokio runtime; rewrote 1 barrier-free.
- `crates/recursive-tui/src/ui/status.rs` — restored `* 100.0` that an
  interrupted `tui-mutants.sh` had mutated to `/ 100.0` and that a later
  `git add -A` committed into HEAD (cache rate was rendering 0%).
- `crates/recursive-tui/src/{app/state,app/mod,app/event_loop,backend,commands,runtime_builder}.rs`
  — ~35 new unit tests to kill pre-existing + new-code mutants surfaced by
  `tui-mutants` (rendering, truncation, selection/style, API-key normalization,
  MCP transport classification, command-panel scroll, failed-Write diff skip).
- `.dev/scripts/tui-mutants.sh` — `--in-place` contamination guard
  (pre-flight clean check + EXIT trap that `git checkout`s any file still
  carrying a `cargo-mutants` marker), `--list` / `--list-files` dry modes,
  and `--jobs N` parallel support (jobs>1 uses copy mode, never touching real
  source).
- `.dev/scripts/salvage-from-transcript.sh` — new. Recovers a rolled-back
  run's code from its transcript.jsonl by extracting apply_patch / write_file
  tool calls into a review bundle (files/, patches/, all.patch, manifest.txt).

**Tests added**: ~35 (state 6, backend 4, commands ~13, runtime_builder 6,
app/mod 3, event_loop 1, run_background 6 net). All touched files now show
0 missed mutants under `tui-mutants`.

**Notes**:
- First whole-crate recursive-tui mutation baseline (parallel, `--jobs 6`,
  52 min for 1340 mutants): 1026 caught, 219 missed, 80 unviable, 15 timeout.
  Debt is concentrated: `app/commands.rs` (108), `ui/command_menu.rs` (31),
  `ui/markdown.rs` (28) = 76% of all missed. Work list lives in
  `mutants.out/missed.txt`.
- Key lesson: `cargo mutants --in-place` does NOT restore source on
  interruption; the leftover marker gets committed by the next `git add -A`
  and is invisible to `git status` once in HEAD. The new EXIT-trap guard
  prevents this; `salvage-from-transcript.sh` reduces rolled-back recovery
  from manual JSONL parsing to one command.
- The prior self-improve flow run was still cycling autonomously (rolled-back
  ≠ stopped) and contending for CPU; had to kill `selfimprove-*` worktree
  processes + the `recursive-flow-*` tmux session before manual work could
  proceed cleanly.

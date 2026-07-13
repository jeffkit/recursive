# Manual edit: loop-session-persistence + stream-json-num_turns-doc

**Date**: 2026-07-13
**Goal**: Land Goal 327 (`recursive loop` session persistence) and Goal 326
(stream-json `num_turns` semantics + Client-mode documentation).
**Files touched**:
- `crates/recursive-cli/src/main.rs` — `run_loop` now wires a `SessionWriter`
  + `SessionPersistenceSink` (and a `CostTracker`) so every wakeup turn appends
  to ONE `transcript.jsonl`; respects `--no-session` / `--name`; finalizes the
  writer even when `runtime.run_loop` errors mid-loop (Crashed status) before
  propagating. `run_loop` signature gained a `session: bool` param, fed from
  `!cli.no_session` at the `Cmd::Loop` call site.
- `crates/recursive-cli/src/cli/output.rs` — new `finish_to_session_status`
  helper (NoMoreToolCalls→Completed, Cancelled→Interrupted, else→Crashed) with
  3 unit tests.
- `crates/recursive-cli/src/cli/claude_json.rs` — module doc-comment documents
  the `--input-format stream-json` Client mode (multi-turn, one session, one
  `result` per turn) and that `result.num_turns` is per-turn (per-query), not
  cumulative. Inline doc on the `num_turns` calculation. No runtime behaviour
  change.
- `e2e/tests/10-loop-mode.yaml` — added `unset RECURSIVE_SESSIONS_DIR` +
  `RECURSIVE_HOME=/tmp/rh-loop` isolation and a `recursive-session:` assertion
  proving the single-turn loop persists a completed session with the
  `write_file` call.
- `e2e/tests/17-loop-mode.yaml` — RESTRUCTURED: the suite now ends the loop
  cleanly (exit 0) and adds an in-suite `recursive-session:` assertion proving
  BOTH turns (schedule_wakeup + write_file) land in ONE completed, finalized
  session. Uses `unset RECURSIVE_SESSIONS_DIR` + `RECURSIVE_HOME=/tmp/rh-loop17`
  isolation and a `/tmp/sessions-loop17` capture. See Notes for the root cause
  this uncovered.
- `e2e/fixtures/17-loop-mode.json` — RESTRUCTURED (4 entries) so turn 1 ONLY
  schedules a wakeup and ends, and the wakeup turn (turn 2) writes the output
  file and ends — i.e. the suite now actually exercises the wakeup/reschedule
  path instead of turn 1 doing everything. Arg names fixed to the tool spec
  (`delay_secs`/`reason`/`prompt`); turn-2 entries keyed on the unique
  substring `Continue` (absent from the goal) with `hasToolResult:true` (the
  loop shares the transcript, so turn 2's first call already carries turn 1's
  tool result). See Notes.
- `.dev/goals/326-stream-json-num-turns-semantics.md`,
  `.dev/goals/327-loop-session-persistence.md` — goal docs (carried into the
  worktree).

**Tests added**:
- `cli::output::tests::finish_to_session_status_maps_cancelled_to_interrupted`
- `cli::output::tests::finish_to_session_status_maps_success_to_completed`
- `cli::output::tests::finish_to_session_status_maps_errors_to_crashed`
- E2E: `10-loop-mode` new case "loop persisted a valid completed session with
  the write_file call".
- E2E: `17-loop-mode` new case "loop persisted ONE multi-turn session with both
  turns' tool calls" (schedule_wakeup + write_file, minMessages 8, completed,
  finalized) — the multi-turn-same-session property is now asserted in-suite.

**Quality gates**: `cargo fmt --all --check` clean; `cargo clippy
--all-targets --all-features -- -D warnings` clean; `cargo test --workspace`
all pass (0 failed). E2E (ArgusAI 0.14.2, MCP path) regression: loop 2/2,
loop-schedule 3/3, claude-json-stream 12/12, smoke 3/3. Full E2E: 103 passed /
12 failed / 48 skipped — the 12 failures are pre-existing and unrelated
(confirmed by re-running at the parent commit 751f4b2 with the 17-loop fixture
restructure stashed: identical 12 failures; they span HTTP-API, SDK,
bash/sandbox, compaction, skill, goal-loop-API, deferred-tool, utility — none
touch `recursive loop` or the 17-loop fixture).

**Notes**:
- `run_loop` mirrors `run_once`'s session wiring EXCEPT the `event_sink`
  carries ONLY `SessionPersistenceSink` (no `ChannelSink`) — loop has no stdout
  printer draining a channel, so attaching one would deadlock the unbounded
  channel. The runtime is `drop(runtime)` before `finalize_session_writer` so
  the sink releases its Arc clone and `Arc::into_inner` succeeds (same pattern
  as `run_once`).
- Critical bug found & fixed during E2E: when `runtime.run_loop` errors mid-loop
  (e.g. provider 404 on a wakeup turn), the original `?` skipped session
  finalization, leaving the session `active` (unresumable-looking). Now the
  error is captured, the writer is finalized as `Crashed`, then the error
  propagates. Verified during debugging with a forced mid-loop 404: the session
  is left `crashed` (finalized) instead of `active`. (The committed 17-loop
  fixture no longer 404s — turn 2 succeeds — so this path is now exercised by
  other error scenarios, but the finalization guard remains necessary for any
  mid-loop provider/runtime failure.)
- Multi-turn-same-session property (both turns land in ONE transcript) is now
  asserted IN-SUITE on 17-loop (see the new `recursive-session:` case). The
  earlier claim that this was impossible due to an "argusai setup-exec
  limitation" was WRONG — corrected below.
- ROOT CAUSE of the 17-loop 60s stall (was mis-attributed to argusai): the
  17-loop fixture called `schedule_wakeup` with the WRONG argument names —
  `{"seconds": 0, "message": "..."}` — but the tool reads `args["delay_secs"]`
  (`src/tools/schedule_wakeup.rs`, `unwrap_or(60).clamp(1, 3600)`). The name
  mismatch made `delay_secs` default to 60, so `run_loop` did
  `tokio::time::sleep(Duration::from_secs(60))` before turn 2. That 60s sleep
  exceeded argusai's setup-exec timeout, so the post-loop session-capture
  statements never ran — which looked like "argusai aborts after a non-zero
  command." A minimal argusai repro (`sh -c 'exit 1' || true; echo > marker`)
  proved argusai runs subsequent statements fine after a masked non-zero
  command; the 60s was purely recursive's own sleep. This is NOT an argusai bug
  and NOT a recursive code bug — it was a fixture authoring bug (stale arg
  names). Fixing the arg names (`delay_secs:1`) dropped the suite from ~60s to
  ~1.1s. Lesson: when an E2E suite stalls for a round number of seconds, check
  the fixture's tool-call arg names against the tool spec before blaming the
  harness.
- The fixture was also structurally confused: turn 1 consumed all 3 fixtures
  (schedule_wakeup → write_file → "Done"), so turn 1 wrote the file AND
  scheduled a wakeup, and turn 2 404'd (no fixture left). The restructure makes
  turn 1 only schedule + end, and turn 2 write the file + end, so the loop
  exits 0 (Completed session) and the suite actually tests "wakeup triggers a
  second turn that writes the file."
- `num_turns` (Goal 326): verified against Claude Agent SDK docs —
  `receive_response()` "Receive messages until and including a ResultMessage",
  so each `query()` yields its own `ResultMessage` with per-query `num_turns`.
  Recursive's `num_turns = steps` (per-turn agent-loop step count) is aligned;
  no `build_result` change needed. A stream carrying multiple `result` events
  is correct Client-mode behaviour, not a bug.

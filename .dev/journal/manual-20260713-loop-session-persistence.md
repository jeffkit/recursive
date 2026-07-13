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
- `e2e/tests/17-loop-mode.yaml` — description extended to note why no in-suite
  session assertion is added (see Notes). No behaviour change.
- `.dev/goals/326-stream-json-num-turns-semantics.md`,
  `.dev/goals/327-loop-session-persistence.md` — goal docs (carried into the
  worktree).

**Tests added**:
- `cli::output::tests::finish_to_session_status_maps_cancelled_to_interrupted`
- `cli::output::tests::finish_to_session_status_maps_success_to_completed`
- `cli::output::tests::finish_to_session_status_maps_errors_to_crashed`
- E2E: `10-loop-mode` new case "loop persisted a valid completed session with
  the write_file call".

**Quality gates**: `cargo fmt --all --check` clean; `cargo clippy
--all-targets --all-features -- -D warnings` clean; `cargo test --workspace`
all pass (0 failed). E2E (ArgusAI 0.14.2, MCP path) regression: loop 2/2,
loop-schedule 2/2, claude-json-stream 12/12, smoke 3/3.

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
  propagates. Verified: 17-loop's turn-2 404 now leaves a `crashed` (finalized)
  session instead of `active`.
- Multi-turn-same-session property (both turns land in ONE transcript) is
  verified MANUALLY, not via an in-suite assertion on 17-loop: the 17-loop
  fixture's turn-2 wakeup 404s by design (only 3 fixtures), and argusai's
  setup `exec` does not run statements after a non-zero-exiting command in the
  same step (nor tolerate it via `|| true` / `if` in-step; a 3-step split
  paradoxically dropped the session entirely). Manual run of the 17-loop block
  produced a 7-message transcript containing BOTH `schedule_wakeup` and
  `write_file` tool calls, status `crashed` (finalized) — direct evidence that
  multiple loop turns share one session. 10-loop's E2E assertion covers the
  single-turn completed-session case.
- `num_turns` (Goal 326): verified against Claude Agent SDK docs —
  `receive_response()` "Receive messages until and including a ResultMessage",
  so each `query()` yields its own `ResultMessage` with per-query `num_turns`.
  Recursive's `num_turns = steps` (per-turn agent-loop step count) is aligned;
  no `build_result` change needed. A stream carrying multiple `result` events
  is correct Client-mode behaviour, not a bug.

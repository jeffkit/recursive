# Issue #99 — loop mode 全链内存态：wakeup 不落盘 + 单个 turn Err 终结整条循环

## Date
2026-10-04

## Goal
`.dev` gap issue #99（P1）. Two ways a self-scheduling multi-day loop died:

1. `schedule_wakeup` wrote only an in-process slot
   (`src/tasks/schedule_wakeup.rs:16`) and `run_loop` slept on it; a restart /
   upgrade silently evaporated the pending wakeup — the agent was never woken
   again and only an external observer could notice.
2. `run_loop` did `self.run(&next_goal).await?`, so ONE failing turn (provider
   5xx/429 blip, the classic case being a provider 404 after the provider-level
   `RetryPolicy` exhausted its 2 attempts) ended the whole multi-day loop, and
   the resume path could not recover the original wakeup prompt
   ("Continue from where you left off.").

Note on the motivating case: the provider **404** cited above is classified
*permanent* by `LoopRetryPolicy::is_permanent_http_message` (4xx minus
408/429), so it is deliberately **not** retried — a 404 is the provider's final
word and replaying it only burns tokens. What the retry covers is the
transient blip (5xx/429/timeout/IO).

## Files touched
- `src/tasks/wakeup_store.rs` (new) — durable pending-wakeup record. One JSONL
  line per session directory (`wakeup.jsonl`): `reason`, `prompt`,
  `scheduled_at_ms`, `due_at_ms`. `persist` / `clear` / `load` are the
  in-session API; `take_due_in_workspace(workspace, now_ms)` scans the
  workspace's `<sessions>/<slug>/<session-id>/` dirs for the oldest record whose
  due time has passed, consumes it (at-most-once) and returns it for restore.
  Directories whose `.lock` is held by a live process are skipped: their record
  belongs to a loop that is still running. Writes go through
  `atomic::atomic_write`.
- `src/session/{lifecycle,mod}.rs` — `locked_by_live_process(dir)`: the
  liveness half of `SessionLock::acquire`'s stale-lock recovery, exposed for the
  wakeup scan.
- `src/tasks/mod.rs` — `pub mod wakeup_store;`
- `src/runtime/loop_retry.rs` (new) — `LoopRetryPolicy` (max_retries 4, 5s → 60s
  exponential cap) + `is_retryable(&Error)` classification: transient
  (rate-limit/timeout/http/io) and provider `Error::Llm` are retryable except
  `HTTP 4xx` (minus 408/429) and context-window-exceeded; cancel/budget/config/
  permission/tool-arg errors are not.
- `src/runtime.rs` —
  - `run` split into `run` (hooks + user-message append) and private
    `drive_turn` (kernel turn + event emit + cross-turn compaction + turn
    counter), so a retry can re-drive a failed turn without appending the goal
    twice. Behaviour of the public `run` is unchanged.
  - `execute_kernel_turn` now collects the `MessageAppended` events the
    forwarder already sees and, when the kernel turn fails, folds those
    committed messages back into `self.transcript`. The kernel runs on a COW
    clone, so before this the failed attempt's messages existed only on disk
    (`transcript.jsonl`, push-time persistence): the live transcript was still
    the pre-turn one and a retry restarted the turn from the goal, silently
    re-running its tools. With the fold, the retry *resumes* at the step that
    failed.
  - `run_loop` now: retries a failed turn via `run_turn_with_retry` (only when
    `retry_is_safe` — no assistant output for the turn has landed, so the
    transcript tail is the resume point), persists the pending wakeup
    (`persist_pending_wakeup`) before the sleep, clears it the moment the wakeup
    fires, and clears it again when the loop ends or errors.
- `src/runtime/builder.rs` — `loop_retry(policy)` + `wakeup_store_dir(dir)`
  setters, new `AgentRuntime` fields.
- `src/runtime/tests.rs` — unit tests for `retry_is_safe` and builder wiring.
- `crates/recursive-cli/src/main.rs::run_loop` — restores an overdue wakeup from
  the workspace's sessions as the loop's first goal (`PersistedWakeup::restored_goal`
  merges the original wakeup prompt with the operator's goal) and hands the
  session dir to `AgentRuntimeBuilder::wakeup_store_dir`. Both sides are gated on
  session recording: `--no-session` keeps the old in-memory-only behaviour and
  must not consume (i.e. delete) another session's record.
- `tests/run_loop_wakeup_persist.rs` (new) — end-to-end loop behaviour.

## Tests added
- `src/tasks/wakeup_store.rs` (15 tests) — round trip, single-line overwrite,
  missing/malformed file, `clear` idempotence, directory creation, due-time
  boundary, overdue clamp, `restored_goal` merge (incl. empty prompt), scan
  ordering + `take_due_in_workspace` consume/at-most-once + future-record
  rejection + live-owner skip/release (RECURSIVE_HOME-pinned).
- `src/session/lifecycle.rs` (1 test) — `locked_by_live_process` across
  no-sentinel / own-pid / dead-pid / cross-host / corrupt-sentinel.
- `src/runtime/loop_retry.rs` (9 tests) — backoff schedule/cap/exhaustion,
  retryable vs permanent classification (503/404/401/429/408/context-window/
  cancel/budget/config/tool-args).
- `src/runtime/tests.rs` (3 tests) — `retry_is_safe` per transcript tail (incl.
  the injected-system-note tail), builder wiring of `loop_retry` +
  `wakeup_store_dir`, defaults are off.
- `tests/run_loop_wakeup_persist.rs` (6 tests) — the record is on disk while the
  loop sleeps and gone the moment the wakeup fires; transient failure retried
  without duplicating the goal in the transcript; **mid-turn failure resumes
  instead of restarting** (the failed attempt's tool ran exactly once, the retry
  prompt carries its result, the transcript keeps one copy); retry budget is
  finite (initial + 2, then Err); `HTTP 404` is not retried; a loop that dies
  clears the record before propagating.

## Notes
- Restore is **due-time only**: a not-yet-due record is left on disk rather than
  parking the operator's terminal until the original wakeup time. Restoring
  consumes the record (at-most-once), so a second restart cannot replay work the
  first restart already handed to the agent.
- Clearing the record the moment the wakeup fires is what keeps a *running*
  loop's record from being stolen: during the following turn the record is no
  longer pending (it is that turn's goal), so it must not be restorable. A crash
  mid-turn is recoverable from `transcript.jsonl`.
- The retry deliberately does not replay a turn whose transcript tail is an
  assistant message: that means the turn already produced its answer, so a
  replay would append a second one for work that is already recorded. A kernel
  step-loop failure always leaves a non-assistant tail (it happens while
  dispatching an LLM call, before that step's assistant message is pushed), so a
  mid-turn failure resumes at that call rather than restarting the turn.
- No new dependency (std + serde + existing `atomic`); the loop-retry backoff is
  capped so an operator is never parked at a terminal for an unbounded time.
- Not covered here: the TUI/HTTP `run_event_loop` path and `send_message`-driven
  loops keep their previous semantics — issue #99 asks for loop mode
  (`recursive loop`) specifically.

## Review response (round 2)
- **Replayed side effects (blocking).** Fixed as described above: a failed
  kernel turn's committed messages are folded back into the transcript, so the
  retry resumes at the failed LLM call instead of restarting the turn and
  re-running its tools. Covered by
  `run_loop_resumes_a_mid_turn_failure_without_re_running_its_tools`.
- **Stolen wakeup record (blocking).** Fixed twice over: `run_loop` clears the
  record the instant the wakeup fires, and `take_due_in_workspace` skips session
  directories held by a live process.
- **`--no-session` consuming records (blocking).** Fixed: the restore is gated
  on session recording, matching the persist side.
- **Stale contract comment.** `tests/run_loop_wakeup_persist.rs` no longer cites
  the invariant #1 line budget; the `drive_turn` doc comment no longer claims a
  failed kernel turn duplicates nothing.

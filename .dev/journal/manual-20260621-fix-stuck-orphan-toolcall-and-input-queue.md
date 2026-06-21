# Manual edit: fix stuck-detection orphan tool_use + TUI input queueing

**Date**: 2026-06-21
**Goal**: Fix two TUI/agent bugs surfaced by a web-fetch session that ended
in a permanent HTTP 400 loop and silently swallowed follow-up messages.

## Bug 1 (critical) — stuck detection left orphaned `tool_use` blocks

In `run_core.rs::run_inner`, the sliding-window stuck detector lived **inside**
the `for ... in &results` loop and did `return Ok(outcome)` *before*
`self.push_message(Message::tool_result(...))`. When a multi-call step had a
high error rate (e.g. several `WebFetch` calls returning 404), stuck fired
mid-batch, so the assistant message kept N `tool_call`s while only k<N
`tool_result`s were pushed. That unbalanced transcript was returned as a
normal `Ok` outcome and committed by `runtime.rs::emit_turn_messages`, leaving
orphaned `tool_use` blocks. Every subsequent turn re-sent the corrupted
transcript and the provider rejected it with HTTP 400
("tool_use ids ... were found without tool_result blocks"). Violated
invariant #8.

Fix: record the stuck verdict in a `stuck_finish: Option<FinishReason>` and
push every tool_result first; only after the loop (all calls paired) do we
emit `TurnFinished` and return. Mirrors the already-correct
`PermissionDenialLimit` path.

## Bug 2 — messages submitted mid-turn were silently dropped (no queue)

The TUI backend's `run_turn_select_loop` discarded any `UserAction` that was
not plan/interrupt/shutdown via `Some(_) => {}`, so a `SendMessage` typed
while a turn was running was lost (the UI still echoed the user bubble,
masking the loss). The runtime already has a FIFO `message_queue`, but the
backend never reached it because it was blocked in the select loop.

Fix: buffer mid-turn `SendMessage`s into a `VecDeque<String>` and drain them
FIFO at the top of the worker loop once the current turn completes
(type-ahead queueing). On abort (interrupt/shutdown) the buffer is cleared so
queued input isn't run against the user's wishes. All three
`run_turn_select_loop` call sites (SendMessage, ConfirmPlan, SetGoal) pass the
shared buffer.

## Files touched
- `src/run_core.rs` — defer stuck termination until all tool_results pushed
- `src/tui/backend.rs` — mid-turn SendMessage buffering + FIFO drain
- `src/runtime.rs` — regression test `stuck_detection_keeps_tool_calls_paired`
- `tests/tui_backend_smoke.rs` — regression test
  `messages_submitted_during_running_turn_are_queued_not_dropped`

## Tests added
- `runtime::tests::stuck_detection_keeps_tool_calls_paired` — drives a real
  turn with 3 failing tool_calls under stuck_window=2/rate=1.0 and asserts all
  3 tool_calls have matching tool_results.
- `messages_submitted_during_running_turn_are_queued_not_dropped` — uses the
  MockProvider `on_complete` hook to inject a second message during turn 1's
  first LLM call and asserts both turns produce output.

## Quality gates
- `cargo clippy --all-targets --all-features -- -D warnings`: clean
- `cargo fmt --all`: clean
- `run_core` + `runtime` + `tui_backend_smoke` suites: all green

## Notes
- GitNexus impact on `run_inner`: risk LOW, only direct caller is
  `AgentKernel::run`; the fix is internal (no signature change).
- Pre-existing unrelated failures: 7 tests in `llm::anthropic` / `llm::openai`
  fail on the current working tree. They belong to an in-flight llm streaming
  refactor (`src/llm/{anthropic,openai,chat,mod}.rs` were already `M` before
  this work) — confirmed unrelated: stashing only the provider files breaks
  compilation, showing they're part of a coordinated change set. Not touched.
- Known cosmetic follow-up: while the backend drains a queued message, the TUI
  spinner/turn-state isn't re-armed (no "turn started" UiEvent), so output for
  a queued turn appends without a spinner. Messages are never lost or
  reordered. Issue #2 (scrollback completeness / `u16` scroll truncation) was
  left for a separate focused change.

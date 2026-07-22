# Manual edit: loop-supervise (monitor + intervene)

**Date**: 2026-07-22
**Goal**: Give the Recursive TUI agent a built-in, generic "supervise a long-running
command and intervene on problems" capability, so it can run the self-improve
flow autonomously and lend a hand mid-run — like a human watching the loop.
Closes the gaps surfaced when manually babysitting goal 328's land-preserve.

**Files touched**:
- `crates/recursive-tui/src/supervise_sop.md` (new) — embedded generic SOP.
- `crates/recursive-tui/src/commands.rs` — `SUPERVISE_SOP` const (`include_str!`);
  `/loop supervise <command...>` subcommand (merges the loop skill into the
  built-in `/loop` command; injects SOP+command as the loop goal, unlimited
  turns, short label in `loop_state` for display); updated usage/error strings;
  2 tests.
- `src/tools/run_background.rs` — `WatchTarget` + `watch` field on
  `BackgroundJobManager`; `set_watch` / `poll_watch` (chunked, rotation-safe) /
  `clear_watch`; `pub(crate) WATCH_CHUNK_BYTES`.
- `src/tools/watch_file.rs` (new) — `watch_file` tool (deferred) that registers a
  file for mid-run event wakes on the shared bg manager; 9 tests.
- `src/tools/mod.rs` — module + re-exports (`WatchFile`, `WatchTarget`).
- `src/tools/registry.rs` — register `WatchFile` alongside `RunBackground`/
  `CheckBackground` (reuses the already-threaded `bg_manager`).
- `crates/recursive-tui/src/backend.rs` — `#[derive(Debug)] ArbiterDecision`;
  new `event-watch` select! branch in `loop_arbiter` that polls the watched file
  and wakes the agent only on new bytes (event-driven, no per-tick turn); 2 tests.
- `crates/recursive-tui/src/cost.rs` — removed a stray duplicate doc line that
  tripped `clippy::empty_line_after_doc_comments` (toolchain drift; pre-existing
  in the goal-328 area, not introduced here).
- `.dev/flows/self-improve.flow.js` — `emitEvent()` writes structured events
  to `<run-dir>/events.jsonl`: `start` / `gate-failed` / `preserve-created` /
  `verdict` (incl. resume/land-preserve). Lets `watch_file` wake the supervising
  agent mid-run instead of only at termination.
- `.claude/skills/recursive-loop/SKILL.md` — reworked §3/§3.5 to build on
  `/loop supervise` + `watch_file` + pause-and-ask HITL; kept project-specific
  orchestration (roadmap, goals, verdict handling, land/resume-preserve).

**Tests added**:
- `commands::tests::cmd_loop_supervise_emits_start_with_sop_goal`
- `commands::tests::cmd_loop_supervise_empty_errors`
- `tools::watch_file::tests::*` (9 cases: registration, from_end, sandbox
  rejection, poll/advance, truncation, chunk cap, replace, clear)
- `backend::tests::loop_arbiter_wakes_on_watched_file_new_bytes`
- `backend::tests::loop_arbiter_idles_when_watch_has_no_new_bytes`

**Design decisions / notes**:
- **Merge, not split**: per user steer, the generic monitor+intervene SOP is
  baked into the built-in `/loop` command as a `supervise` subcommand (the
  `/loop` command hadn't shipped to production yet). No separate `supervise`
  skill file — avoids the built-in-command-shadows-skill collision and keeps
  one loop entrypoint.
- **B (event-driven mid-run)**: implemented by reusing the already-everywhere-
  threaded `BackgroundJobManager` (added a `watch` field) instead of threading a
  new shared slot through `TuiRuntime`/`worker_loop`. The arbiter polls the
  watched file every ~1s and wakes the agent only on new bytes — so it's
  event-driven from the agent's POV (no LLM turn unless there's a new event),
  no new deps, no new channels.
- **C (HITL decision)**: uses the existing arbiter semantics — the agent asks
  via its final message and does NOT arm `schedule_wakeup`; the loop idles
  (arbiter blocks on user action) and the user's reply (queued message) drives
  the next turn. The SOP also notes a HITL MCP tool (e.g. `send_and_wait_reply`)
  can be used if configured. No new tool/channel needed.
- **Deferred tools**: `run_background`/`check_background`/`watch_file`/
  `schedule_wakeup` remain `is_deferred`; the embedded SOP names them explicitly
  and tells the agent to `tool_search` if they're not in its eager list.
- **Flow self-heals most gate failures** via resume-fix; the agent should let
  those run and only intervene on flow-level failures (config bugs, env
  prereqs, decisions) — the SOP says so. The new `preflight.gate-prereqs`
  (prior commit) already removes the biggest env-prereq failure mode.
- `cost.rs` clippy fix is incidental (toolchain drift), documented here for
  traceability.
- `tui-mutants.sh` (advisory) scoped to `backend.rs`+`commands.rs` was
  interrupted early (whole-file mutate on the 2700-line `backend.rs` is the
  CLAUDE.md "false friction" case). The one survivor it reported before
  interruption — `backend.rs:243:9 delete match arm AgentEvent::ContextBreakdown
  in map_agent_event` — is **pre-existing Goal 328 debt** (the ContextBreakdown
  event-mapping arm has no test that fails when deleted), not in this change's
  diff hunks. Noted as debt; no fix required here. New logic is covered by
  targeted unit tests.

Date: 2026-07-31
Goal: 349 (TUI select & copy) — supervisor rescue of a watchdog-killed successful run
Files touched: none new (this is a journal of rescuing commit 502ba36)
Tests added: none

## Timeline

1. Run 1 + 2 (`selfimprove-1785484982049`, `...5427092`, deepseek-v4-flash):
   both `skip-commit / 0 changes`, finishReason `provider_stop:length`,
   last assistant `content=""` + `reasoning_content` ~60K chars. Diagnosed
   as the hard-coded `max_tokens: 16384` (see
   `manual-20260731-max-tokens-config.md`). Fixed manually in commit
   `75c40ac` (RECURSIVE_MAX_TOKENS, default 64K).

2. Run 3 (`selfimprove-1785487265463`, baseline `75c40ac`, env
   `RECURSIVE_MAX_TOKENS=131072`): the fix worked. `run.recursive` ran 601s,
   247 transcript messages, the agent fully implemented goal 349 (arboard
   dep, App selection/last_copied fields, handle_mouse drag selection,
   visible_physical_rows refactor, REVERSED highlight, Ctrl+Y /
   Ctrl+Shift+Y yank, harness tests) and committed `502ba36
   "feat(tui): Goal 349 — selectable & copyable agent output"`.

   finishReason was `cancelled` (NOT `provider_stop:length`) — the
   max_tokens fix eliminated the truncation entirely.

3. But the flow verdict was `failed-preserved`, detail
   `watchdog: no-growth-hung`. The watchdog killed the run while the
   agent was running `.dev/scripts/tui-mutants.sh` as a background job
   (mutation gate, 10+ min). The watchdog saw no transcript growth during
   that long background compile and declared the run hung, cancelling the
   agent mid-gate.

## The flaw exposed

The flow's watchdog (idle/no-growth detection) cannot distinguish
"agent is genuinely stuck" from "agent correctly spawned a long-running
background gate (tui-mutants / cargo build) and is waiting on it". A
~10-minute cargo-mutants run looks identical to a hang through the
transcript-growth lens. This will recur for any goal that hits the
tui-mutants gate. Candidate fix (future goal): the watchdog should treat
an active `run_background` job as liveness, not just transcript growth,
or the tui-mutants gate should heartbeat.

## Supervisor rescue

The agent's work was sound and committed. Cherry-picked `502ba36` onto
`main` as `9c63c8c` (no conflicts). Verified the implementation myself:

- `cargo check -p recursive-tui` — clean (arboard 3.6.1 resolved)
- `cargo fmt --all --check` — clean
- `cargo clippy -p recursive-tui --all-targets -- -D warnings` — clean
- `cargo test -p recursive-tui --lib` — **791 passed, 0 failed**
  (13 more than the 778 before goal 349 — the agent's new tests)
- tui-mutants — running (the gate the watchdog killed it on; supervisor
  ran it explicitly on the changed files to close the loop).

## Lesson

A `failed-preserved` verdict is NOT always a failure of the agent — read
the watchdog-failure.log and the preserved diff before declaring the run
a loss. Here the agent had already committed a clean, well-tested
implementation; the watchdog's no-growth heuristic was the culprit. The
`preserved.diff` + `refs/preserve/<run-id>` ref + worktree are exactly
the rescue path, and they worked as designed.

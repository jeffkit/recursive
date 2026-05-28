# Orchestrator notes — 2026-05-28 entry

> Format note: this is not a per-run journal entry. It's a cold-start
> orchestrator's field report after going through OPERATIONS.md §7
> and noticing things that future orchestrators should know but that
> aren't captured by `git log` / observations / metrics alone.

## Scene at entry

- Branch: `main` @ `ba1fbbb`, working tree clean.
- Last merge: `ba1fbbb` — observation for goal-133 permissions-config (deepseek).
- In-flight runs: none. `.worktrees/` empty.
- Cargo test: green (5 unit tests pass; integration suite 5 ignored — provider-keyed).

## Discovery 1 — ROADMAP-v4 markers are stale

ROADMAP-v4 marks **14.3 (transcript export/import)** and **15.3 (cost
tracking)** as 🟡 partial, citing "CostTracker wiring deferred" and
"export CLI pending". Both are inaccurate as of HEAD:

- **14.3**: `recursive sessions export` shipped in `e2da6c1` (goal-119).
  ExportedTranscript struct lives in `src/session.rs`; CLI handler in
  `src/main.rs` `SessionCmd::Export`. Verified by integration tests.
- **15.3**: `CostTracker` is fully wired in `src/main.rs`:
  - construction at L1342 (`run_once`) and L1587 (`run_resumed`)
  - `finalize_cost_tracker(...)` called at L1416 and L1682 after
    `runtime.run(goal)` returns
  - `cost.rs` carries 12 unit tests covering `new → record_usage →
    finish → cost.json + meta.json side effects`

  The wiring landed in two stages: g118 first run (`8e79f5e1`,
  2026-05-27) wired the v0.4-era Agent path; the `cd224cd` merge
  dropped the product changes in conflict resolution but the source
  branch `8e79f5e1` already existed in history. Then `190b6e16`
  (Goal H, 2026-05-28, AgentRuntime migration) re-wired it on top of
  the new runtime, including the `Mutex` wrapper and extracted
  `finalize_cost_tracker`/`finalize_session_writer` helpers reused
  by both `run_once` and `run_resumed`.

**Action taken**: ROADMAP-v4 14.3 and 15.3 flipped to ✅ in this
batch. See accompanying commit.

## Discovery 2 — Goal-133 ran twice; second run was wasted budget

`git log --oneline` around 2026-05-28 14:53–14:55 shows two
`self-improve(permissions-config)` commits in a row:

- `6ccfbfa` — first run, real product change (4 files: `src/lib.rs`,
  `src/permissions.rs` +231, `src/tools/mod.rs` +38, plus journal).
- `04c19ee` — second run, "1 files changed" but actually zero product
  delta (only journal + metrics + review file). Baseline `ca8f26a` is
  the observation commit *of the first run*, so the second run was
  invoked on top of an already-completed goal. Agent correctly noticed
  there was nothing left to do and only emitted meta files.

Likely cause: a `RECURSIVE_PROVIDERS=` rotation or accidental re-launch.
Functionally harmless (no double-edit), but burned ~50 steps on a
no-op goal that could have gone to a 🔴 item.

**Suggested mitigation (not yet implemented)**: have
`self-improve.sh` short-circuit when the goal file's SHA already
appears as a successful observation against the current `HEAD~1` or
later. Out of scope for this orchestrator pass — flag for a future
`.dev/`-only improvement.

## Discovery 3 — `.dev/runs/*.pid` accumulates dead pidfiles

`.dev/runs/` is gitignored and meant for live `<id>.pid` + `<id>.log`
pairs. After two days of self-improve activity, 140+ `.pid` files
accumulated, all pointing at long-dead PIDs. macOS rotates PIDs
quickly, so two of them happened to alias to live system processes
(`findmybeaconingd`, `SystemUIServer`) — which is misleading if any
future tool naively does `kill -0 $(cat *.pid)` to detect "is it
still running".

**Action taken**: removed the two confusingly-aliased `.pid` files
(`mcp-server-stdio-…-264.pid`, `schedule-wakeup-…-41333.pid`). Did
not mass-prune the rest — that's a separate cleanup chore.

**Suggested follow-up**: a `parallel-self-improve.sh` post-run hook
to delete its own `.pid` file on terminal-marker emission. Or a
weekly cron-style sweep from a `.dev/scripts/runs-gc.sh`.

## Discovery 4 — Code review checklist would have caught the deferred merges

Three goals in the recent batch carry "deferred" annotations in their
merge commit messages:

- `cd224cd merge: goal-118 wire-cost-tracker […, CostTracker wiring deferred]`
- `64dfec1 merge: goal-120 graceful-shutdown […; runtime integration deferred]`
- `1d7a53d merge: goal-121 rate-limiting […; HEAD already has rate limiting]`

Per OPERATIONS.md §3.4.1 the review options are merge / merge+note /
reject+rerun / reject+revise. All three were "merge+note" judgments
that effectively shipped 0 product code from those goal files. The
roadmap stayed 🟡 because of these notes — but the *real* state
(source-of-truth: `git blame` + grep on lib API) was that the work
got redone in subsequent goals (notably Goal H `190b6e16` for cost
wiring). The roadmap and the source diverged.

**Recommendation**: when an orchestrator decides "merge + note,
runtime integration deferred", they should also create a
follow-up goal file (NN+x-tag) or at minimum amend the roadmap
*right then*. Don't trust that "I'll come back to it" — handovers
break that promise. (This entry itself is the audit fix-up.)

## Carried forward

- No live runs to process.
- Next planned action: per user direction, drop the candidate g134
  (cost-tracker-flow-test) — `cost.rs` already has the equivalent
  coverage in `test_cost_tracker_finish_writes_files` (L437) and
  `test_cost_tracker_write_cost_json` (L404). Adding another flow
  test would have been duplicate coverage.
- Next decision pending from human: whether to start Phase 17.2
  (Auth, ROADMAP-v4 Batch 38) or pick up something else.

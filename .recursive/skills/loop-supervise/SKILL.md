---
type: Skill
name: loop-supervise
description: "Monitor+intervene playbook for the event-driven /loop. Use when the user wants to run a long-running command and watch it, intervening only when it needs a decision or fix it can't make itself. Generic pattern first; includes a dedicated section for Recursive's own self-improve flow (.dev/flows/self-improve.flow.js) with the flow-specific launch args, event schema, verdict handling, and intervention rules — self-contained, no other skill required."
mode: trigger
triggers: supervise, monitor, watch, 盯, 盯着, 盯住, 长跑, loop, 跑着, 看着
---

# loop-supervise — Monitor + intervene for the event-driven loop

## When to use

The user wants to **run a long-running command and watch it**, stepping in only
when it needs a decision or a fix the command can't make itself — and otherwise
letting it run to a terminal outcome. This skill teaches the *pattern*; the
command itself comes from the user's natural-language prompt.

This is the generic pattern. It also carries a **dedicated section below**
("Recursive self-improve flow — flow-specific rules") for Recursive's own
self-improve flow (`.dev/flows/self-improve.flow.js`): the launch args, event
schema, verdict handling, and the flow-specific traps that the generic SOP
doesn't cover. The flow section is self-contained — follow it when supervising
a self-improve goal; no separate skill is needed.

## Tools (use them; if one isn't in your eager tool list, `tool_search` for it by name)

- `run_background` — spawn the command non-blocking; you get a `job_id`. The
  loop arbiter is woken **automatically when this job terminates**
  (success / fail / timeout).
- `check_background` — poll a job's status/output. Captured stdout/stderr is
  capped at 128 KB; for long logs, read the file the command tees to instead.
- `watch_file` — register a file (e.g. an events log) for **mid-run event
  wakes**. The arbiter polls it and wakes you only when new bytes appear — this
  is how you get *timely* intervention without burning a turn every tick.
- `schedule_wakeup` — your fallback heartbeat. Call it at end of turn to re-arm
  the next wake after N seconds (1–3600). If you arm neither a wakeup nor a
  watch, and no bg job is pending, the loop idles until the user speaks.
- `stop_loop` — end the loop yourself. Call it when the supervised command has
  reached a final outcome and you've reported the verdict, **or** when the user
  asks to stop / exit the loop in natural language ("停", "stop", "退出循环").
  The loop stops after the current turn; the user doesn't need to type
  `/loop stop`.

## SOP

1. **Derive the command.** From the user's natural-language prompt, work out
   the actual shell command to run. If it's ambiguous, ask before launching.
2. **Launch.** Run the command via `run_background`, teeing output to a known
   log file: `run_background` with command `sh -c '<command> 2>&1 | tee <log-path>'`.
   Pick `<log-path>` next to the command's run dir.
3. **Arm event-watch.** If the command emits structured events to a file (one
   JSON event per line), call `watch_file` on that file so you're woken on each
   event. Otherwise `watch_file` the tee'd log (you wake on each new chunk).
4. **Arm a heartbeat.** `schedule_wakeup` with a delay matching the command's
   cadence — long enough that idle ticks aren't pure overhead (e.g. 120–300s).
   This is your safety net if neither bg-completion nor watch fires.
5. **On each wake** (bg-complete / event-watch / heartbeat / user):
   - **Probe liveness FIRST.** Before concluding "healthy / no intervention",
     verify the supervised process is actually still alive — a **dead**
     process produces no new log/event bytes, which looks identical to
     "alive but slow". Check the `run_background` job status (or, for a
     tmux/nohup launch, `pgrep -f <run-id>` / the tmux pane's foreground
     process). Only say "healthy, no intervention" if the process is alive
     OR you've seen a terminal event/verdict. If the process is gone AND
     no terminal event (e.g. `verdict` / `fatal` / a non-zero exit) was
     emitted AND `state.json` still says `running` → that's a **crash,
     intervene** (see below), not "idle". This is the single most
     important check — without it you will narrate "No intervention
     needed" over a corpse (observed 2026-07-23: a flow crashed in
     preflight, supervisor idled indefinitely watching its corpse's
     events.jsonl).
   - Read the new log lines / event payload since last check.
   - **Healthy progress** (process alive OR terminal event seen) → re-arm
     the heartbeat (maybe lean the delay longer). Do NOT intervene.
   - **Crashed** (process gone, no terminal event, state still `running`)
     → **intervene**: read the tmux pane / tee'd log tail for the stack
     trace, diagnose, apply the minimal fix (PATH/dep/config), then
     re-launch or resume. Do NOT just re-arm the heartbeat over a dead
     process.
   - **Recoverable problem** (missing prerequisite, transient error, a config
     the command can't fix itself) → **intervene**: diagnose (read the command's
     source/config), apply the minimal fix (edit, install dep, start a service),
     then re-launch or resume. Prefer the command's own resume mechanism if it
     has one.
   - **Decision only a human can make** (an opt-out policy choice, a destructive
     action, an ambiguous spec) → **ask**: state the question crisply as your
     final message and **do NOT arm `schedule_wakeup`** — the loop pauses, and
     the user's reply drives the next turn. If a HITL MCP tool (e.g.
     `send_and_wait_reply`) is in your tool list, you may call it to block for a
     reply instead.
   - **Command terminated** → read its verdict / exit code, handle the result,
     then stop (see step 6).
6. **Stop.** When the supervised command has reached a final outcome and you've
   handled it, call `stop_loop` (the loop exits after this turn) and report the
   outcome to the user. Likewise call `stop_loop` if the user asks in natural
   language to stop / exit the loop. Do not arm any further wake.

## Discipline

- Don't intervene on every hiccup — only on problems the command can't
  self-heal. Many long-running commands retry internally; let them.
- **For gated commands** (a test/lint/build suite run in stages with an
  `onFail` policy): learn the policy before judging. `onFail: resume-fix`
  means a red gate is fed back to the worker as more work — the worker is
  *already* fixing it, often by re-invoking the very gate script you see
  "failing" in its transcript. That is the mechanism working, not the worker
  flailing. Step in only when the fix-round budget is exhausted (watch the
  `gate.<name>.fix-N` counter and the configured `MAX_FIX_ROUNDS`) or when
  the failure is environmental and the worker structurally cannot fix it
  (e.g. a missing service it can't install).
- Keep interventions minimal and surgical.
- In one or two lines per wake, note what you observed and what you did, so the
  user can follow along.

## Recursive self-improve flow — flow-specific rules

The generic SOP above covers launch → watch → intervene → stop. When the
supervised command is **Recursive's own self-improve flow**
(`.dev/scripts/launch-flow.sh` → `.dev/flows/self-improve.flow.js`), these
extra rules apply. They are distilled from real supervised runs; the generic
SOP alone will steer you into a dozen avoidable traps.

The flow runs inside a **tmux session**, is **resumable by `--run-id`**, and
writes everything under `<repo>/.flowcast/runs/<run-id>/` (`state.json`,
`run.log.jsonl`, `report.md`, `system-prompt.md`, `transcript.json`).

### Step chain (so you know what phase you're in)

```
preflight.* (baseline capture, build, baseline-tests, worktree, provider ping, gate-prereqs)
  → run.recursive   (the agent edits code in an isolated git worktree — the LONG phase, 10–25 min)
  → gate.test/clippy/fmt/e2e + tui/agent/cli presence & mutants + flow-watchdog
  → review          (cross-provider self-review)
  → commit.prep / commit.land  (lands on main via fast-forward)
  → verdict
```

Verdicts: `committed` (all green + review passed → on `main`) ·
`failed-preserved` (gate/review loop exhausted OR watchdog killed the agent →
worktree + `refs/preserve/<run-id>` kept, **not** rolled back) · `skip-commit`
(no edits / `--no-commit` / reviewer unavailable) · `panic-preserved`.

### Tick cadence by phase

Preflight is fast (~30–90s) → heartbeat 60–90s. `run.recursive` is the long
phase (10–25 min for a real change) → heartbeat 150–240s. Gate phase is
bursty → heartbeat 60–120s. Lean longer when stable, shorter after a state
change. At launch, **capture run-id + tmux session name + log path** — you'll
need all three, and the log path changes on every relaunch (trap 2 below).

Read progress in one shot from `state.json`:

```bash
python3 -c "import json;d=json.load(open('<repo>/.flowcast/runs/<run-id>/state.json'));print(d['status'],d.get('currentStep'),d.get('verdict','-'),d.get('failedStep','-'));print([s['key'] for s in d['steps'] if s['status']=='done'])"
```

### Flow-specific traps (do not learn these the hard way)

1. **Never pre-compile before launch.** The flow's `preflight.build` step
   builds `recursive-cli` itself. If you run `cargo build --release` in
   parallel, you contend on the `target/` directory lock and `preflight.build`
   fails with a misleading error (`invalid character ' ' in package name:
   ' recursive-cli'` — a corrupted lock state, not a real package-name bug).
   Let the flow own the build. (Observed 2026-07-31.)

2. **Each launch writes a NEW timestamped log file.** `launch-flow.sh`
   generates `.flowcast/logs/flow-<TS>.log` afresh on every invocation, so
   after a resume/re-launch the previous log path is stale. Derive the current
   log from the run dir's `events.jsonl` mtime, or glob the newest
   `.flowcast/logs/flow-*.log` — never assume the last run's path still
   applies.

3. **Resume requires `--goal-file` again.** `--run-id <id>` alone errors with
   `缺少 --goal 或 --goal-file`. The run-id reuses the run *directory* for
   outputs; it does **not** restore the goal text. Always pass both:
   `launch-flow.sh --run-id <id> --goal-file .dev/goals/NN-*.md --provider ...`

4. **The dirty-tree guard has TWO layers.** `launch-flow.sh` checks
   `git status --porcelain`, AND flowcast's own `withSelfModGuard` re-checks
   at `preflight.baseline`. Loosening the launcher alone is useless.
   **Commit every goal draft (even untracked) before launching** — there is
   no shortcut.

5. **Diagnose without polluting `main`.** The flow refuses a dirty `.dev/`
   (`withSelfModGuard`), so any patch to `.dev/flows/*.js` must be committed
   before it can run — and forgotten `wip(diag)` commits then sit on `main`
   ahead of the real goal commit (observed: two throwaway diagnostic commits
   polluted history). Prefer a **standalone reproducer in `/tmp`** (a CJS
   `node -e` or `.js` file) over patching the flow script. If you must patch
   the flow, commit → run → `git revert` (never `reset`; the goal commit sits
   on top and revert keeps history honest).

### Gate semantics (know the `onFail` policy before judging a red)

- **`onFail: resume-fix` means the worker is already fixing it.** A red gate
  is fed back into the agent's transcript as more work — the agent re-invokes
  the very gate script you see "failing". Watch the `gate.<name>.fix-N`
  counter in the done list. Gate run count = 1 (flow's first) + N fix rounds;
  `MAX_FIX_ROUNDS` defaults to **3**. Step in only when the counter is
  exhausted (→ `failed-preserved`) or the failure is environmental (a missing
  service the agent structurally can't install).
- **`gate.e2e.fix-1` is NORMAL for any goal touching `src/`** — expect it,
  don't panic, don't intervene. The first e2e run often times out because the
  docker build eats the whole gate timeout; on fix-1 the image is cached and
  it passes. Only worry at `.fix-2`/`.fix-3` → `failed-preserved`.
- **e2e is the cost center.** Any goal touching `src/` or `crates/*/src/`
  pays a docker build (the e2e image compiles recursive + AWS SDK). The
  cargo-chef 3-stage `e2e/Dockerfile` caches the 588-dep cooker layer, so a
  src/ change is ~1 min once the cache is warm; tests/docs/`.dev/`-only goals
  **skip e2e entirely** (diff-scope short-circuit, ~400ms). **Batch
  pure-test/docs goals** — they finish in minutes while an e2e-heavy goal
  churns. Never re-add `COPY src` to the planner stage; it invalidates the
  cooker cache and reverts every worktree build to ~12 min.
- **A gate that completes in ~100ms is NOT passing — it's broken.** Every
  mutants gate was silently false-green for a period: `gates.json` invoked
  bash-only scripts via `sh` (abort at parse time), and a `trap` re-emitted
  `$?` captured inside the trap (0), masking the abort as passed. If any gate
  shows suspiciously fast completion (<1s for something that compiles), grep
  its `result.output` in `run.log.jsonl` for `syntax error` / `command not
  found` / `unexpected token`. A real cargo-mutants run takes minutes.

### Verdict handling & rescue

- **`committed`** → confirm it actually landed:
  `git -C <repo> merge-base --is-ancestor <commit> main && echo ON_MAIN`.
  Then **verify the product yourself** — a green workspace doesn't prove the
  agent added the test the goal required. Run the goal's headline test BY
  NAME (`cargo test <name-substring>`) and re-run the relevant test subset
  rather than trusting the gates.
- **`failed-preserved` ≠ broken code.** This verdict means a gate/review loop
  exhausted OR the `no-growth-hung` watchdog mis-killed the agent during a
  synchronous finishing move (writing the journal, a final `cargo test`) that
  produced zero transcript growth. The agent's complete, self-verified edit
  survives in `refs/preserve/<run-id>` + a `preserve: <reason>` commit in the
  worktree. Decide:
  - **Watchdog mis-fire, code looks correct** (the common rescue): read the
    failure log to confirm, then cherry-pick the preserve commit. **Scan for
    cargo-mutants contamination first** — `grep -rln "changed by
    cargo-mutants" src/ crates/` and `git checkout --` every hit; an
    interrupted `--in-place` run leaves `||`→`&&`-style mutations that
    survive cherry-pick (two real incidents). Then independently run the
    three gates (`fmt --check`, `clippy -D warnings`, `test`). Green → amend
    the message to `feat/fix(...): Goal NNN — …`; red → `git reset --hard
    ORIG_HEAD` and report. Do NOT re-run the goal from scratch — it burns
    another 25 min and may hit the same watchdog.
  - **Genuinely broken after MAX_FIX_ROUNDS**: leave it preserved, read the
    failure context, write a follow-up goal. Do NOT cherry-pick code that
    failed its own gates.
- **`skip-commit`** → read `report.md` detail (no edits / `--no-commit` /
  reviewer unavailable). If the reviewer was unavailable the changes are kept
  uncommitted — report to the user for manual review.
- **`panic-preserved`** → the agent process panicked; the scene is kept for
  diagnosis. Do not roll back; read the transcript and report.
- Clean up after: `git -C <repo> worktree remove .worktrees/<run-id> --force`
  (flow usually does this for `committed`; `failed-preserved` leaves it at
  `.worktrees/preserve/<run-id>` — remove it yourself), `tmux kill-session`
  for leftovers, delete `/tmp` scratch files, `git revert` any diagnostic
  commits you left on main.

### Supervisor discipline (goal-writing & stance)

- **Goals change the AGENT's source, not the scaffolding.** A self-improve
  goal may only edit `src/`, `crates/*/src/`, `Cargo.toml`, `tests/`, `e2e/`.
  `.dev/scripts/*.sh`, `.dev/flows/*.js`, `.flowcast/gates.json`, and the
  skill files are supervisor infrastructure — "the examinee rewriting the
  exam". If a cycle finds a scaffolding bug, fix it with a direct commit, not
  a goal.
- **Write goals to be unambiguous and prescriptive** — the agent only has the
  goal text. Each goal: **Why** (root cause with file:line), **Scope** (do
  exactly this — numbered steps with code), **Files NOT to touch**, **Tests**
  (name the headline regression test), **Acceptance** (exact gate commands),
  **Notes for the agent** (invariant #1 trap, API signatures, ordering). A
  design-principle check that the change does NOT branch
  `run_core.rs::run_inner` is the #1 way goals survive review. Mirror a
  recent same-flavour goal's format.
- **Sequence goals by leverage, mix scopes**: real bugs > invariant/gate
  holes > test gaps > cleanup. Deliberately mix src/ and tests-only goals in
  a batch — tests-only skip e2e and keep momentum while an e2e-heavy goal
  churns.
- **Stop an agent that over-spends its budget self-checking.** If
  `run.recursive` runs >2× longer than similar goals AND the log shows the
  agent polling a long background job (`cargo mutants`, `cargo test --all`,
  a big build) instead of editing, it has finished writing and is
  over-verifying (zero transcript growth → watchdog-bait). Read the worktree
  diff; if the changes look complete, stop the flow (`tmux kill-session` +
  `pkill`), rescue the worktree, and verify yourself — the gate phase exists
  to run the expensive checks. (Incident: goal 373, rescued as `82d7ccb`.)
- **Don't push unless the user asks.** Self-improve commits land on the local
  repo's `main`; "land" and "publish" are different actions.
- **All git targets the recursive repo, never the surrounding monorepo.**
  When supervising from a monorepo root, resolve the recursive root first
  (`[ -f .dev/flows/self-improve.flow.js ] && RR=. || RR=recursive`) and run
  every git/launch command from there.

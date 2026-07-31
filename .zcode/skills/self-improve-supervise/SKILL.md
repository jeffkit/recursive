---
type: Skill
name: self-improve-supervise
description: "ZCode-as-supervisor playbook for running Recursive's self-improve flow (.dev/flows/self-improve.flow.js). Use when the user wants YOU (not recursive's own agent) to drive a self-improve goal end-to-end: write the goal, launch the flow, monitor it, intervene only when it can't self-heal, and report the verdict. Polling-driven (ZCode has no event-driven loop — unlike recursive's own loop-supervise/recursive-loop skills, which assume run_background/watch_file/schedule_wakeup)."
mode: trigger
triggers: self-improve, self improve, 自改, 跑一个goal, 跑一个 goal, 带跑, supervisor, 督战
---

# self-improve-supervise — ZCode drives Recursive's self-improve flow

## When to use

The user wants **you (ZCode)** to act as supervisor and run a Recursive
self-improve goal end-to-end: write a goal file, launch
`.dev/scripts/launch-flow.sh`, watch it to verdict, and intervene only on
problems the flow/agent can't self-heal. The product of the run is a code
change in the `recursive` sub-repo.

> **This is the ZCode/polling version.** Recursive ships its own
> `loop-supervise` / `recursive-loop` skills for recursive's *own* agent
> kernel — but those assume event-driven tools (`run_background` returning a
> job_id that auto-wakes you, `watch_file`, `schedule_wakeup`, `stop_loop`,
> `tool_search`). **ZCode has none of those.** You monitor by
> `sleep` + reading `state.json` / log files. This skill is built around
> that reality, plus the hard-won lessons from real supervised runs.

## What you're supervising (mental model — read once)

`.dev/flows/self-improve.flow.js` is a step chain that runs **inside a tmux
session** (launched by `launch-flow.sh`). It is **resumable by run-id** and
writes everything under `<repo>/.flowcast/runs/<run-id>/`. The step chain:

```
preflight.*  (baseline capture, build, baseline-tests, worktree, provider ping, gate-prereqs)
  → run.recursive       (the agent edits code in an isolated git worktree)
  → gate.test/clippy/fmt/e2e + tui/agent/cli presence & mutants + flow-watchdog
  → review              (cross-provider self-review)
  → commit.prep / commit.land   (lands on main via fast-forward)
  → verdict
```

Verdicts: `committed` (all green + review passed → on main) ·
`failed-preserved` (gate/review loop exhausted → worktree + `refs/preserve/<run-id>`
kept, **not** rolled back) · `skip-commit` (no edits / `--no-commit` /
reviewer unavailable) · `panic-preserved`.

**Key gate semantics (this bit me once — learn it):** gates have an `onFail`
policy. `onFail: resume-fix` means: when the gate goes red, flow does **not**
just re-run the gate itself — it **feeds the failure stderr back into the
agent's transcript** and lets the agent keep running (you'll see the agent's
own `step N` invoking the gate script, e.g. `e2e-gate.sh`). So "the agent is
running e2e" during a fix-round is **flow-directed**, not the agent acting on
its own. Gate run count = `1 (flow's first run) + N resume-fix rounds`. When
green, it stops. `MAX_FIX_ROUNDS` defaults to **3**.

## Tools you actually have (ZCode)

| Need | Tool | Notes |
|---|---|---|
| Launch the flow | `Bash` (foreground, the launcher returns fast) | `launch-flow.sh` backgrounds into tmux and returns within ~15s |
| Wait between checks | `Bash` with `sleep N` | your only "heartbeat". No `schedule_wakeup`. |
| Read progress | `Read` / `Bash` (`cat`/`python3 -m json.tool`) on `state.json`, `run.log.jsonl`, the log file | pick paths from `run-id`, don't hardcode |
| Inspect the agent's work mid-run | `Bash` `git -C <worktree> diff` | read-only on the worktree is safe |
| Kill a stuck/hung flow | `Bash` `tmux kill-session -t <name>` | `tmux ls` to find the name |
| Background a long watch | `Bash` `run_in_background: true` | optional; a single `sleep` block is usually simpler |

You do **not** have: `run_background` (job_id), `check_background`,
`watch_file`, `schedule_wakeup`, `stop_loop`, `tool_search`. Do not write
SOP text that calls for them.

## SOP

### 0. Decide placement & read the contract (before writing the goal)

- This is a **sub-repo** change. All git operations target the `recursive`
  sub-repo (`git -C recursive ...`), never the infra4agent monorepo root.
- Read `<recursive>/AGENTS.md` + `<recursive>/.dev/AGENTS.md` for source
  invariants (the kernel must stay small; no `unwrap()`; finish reasons are
  data; etc.). A goal that violates an invariant will burn fix-rounds.
- Skim `mona.yaml` / `docs/ARCHITECTURE.md` at the monorepo root only if the
  goal crosses sub-repos (it usually shouldn't for a recursive self-improve).

### 1. Write the goal file

- Next number = `max(ls .dev/goals/ | grep -oE '^[0-9]+') + 1`. Mirror the
  format of a recent same-flavour goal (TUI goals look different from
  provider/tool goals — read one neighbour, e.g. `349-*` for TUI).
- A good goal has: **Why** (root cause with file:line), **Scope** (do exactly
  this, no more — numbered steps with code), **Files NOT to touch**, **Tests**
  (name the headline regression test), **Acceptance** (the exact gate
  commands), **Notes for the agent** (traps: invariants, flaky-timing guidance,
  git discipline). The agent only has the goal text — be unambiguous.
- **Design-principle check** block at the top: explicitly state the change
  does NOT branch `run_core.rs::run_inner` (invariant #1). This is the #1 way
  goals get rejected by review.

### 2. Commit the goal (clean tree is mandatory)

```bash
git -C recursive add .dev/goals/NN-*.md
git -C recursive commit -m "dev: add goal NN — <one line>"
```

`launch-flow.sh` **refuses to start if the worktree is dirty**
(`withSelfModGuard`). The goal file must be committed first. Verify:
`git -C recursive status --porcelain | wc -l` → must be `0`.

### 3. Launch (and capture the run-id)

```bash
cd recursive
.dev/scripts/launch-flow.sh \
  --goal-file .dev/goals/NN-*.md \
  --provider deepseek \
  --hitl wecom
```

The launcher prints three things you must **capture into your todo/notes**:
- `run-id` (e.g. `selfimprove-1785...` or your `--run-id`)
- tmux session name (e.g. `recursive-flow-20260731T...`)
- log file path (`.flowcast/logs/flow-<TS>.log`)

> **Do NOT manually pre-compile before launching.** The flow's
> `preflight.build` step builds `recursive-cli` itself. If you run
> `cargo build --release` in parallel, you will contend on the `target/`
> lock and `preflight.build` fails with a misleading error (observed:
> `invalid character ' ' in package name: ' recursive-cli'` — a corrupted
> lock state, not a real package-name bug). Let the flow own the build.

`launch-flow.sh` auto-attaches a cross-provider reviewer (deepseek ↔
deepseek-pro) unless you pass `--reviewer-provider`/`--no-review`.

### 4. Monitor — polling loop (your only mechanism)

There is no event wake. You alternate: `sleep` a while, then probe. Arm each
"tick" as a single `Bash` call that sleeps **then** reads state, so you spend
one tool call per tick, not two.

**Tick cadence:** preflight is fast (~30–90s) → sleep 60–90s. `run.recursive`
is the long phase (10–25 min for a real change) → sleep 150–240s. Gate phase
is bursty → sleep 60–120s. Lean longer when stable; shorter after a state
change.

**Per tick, read in this order (one python one-liner over `state.json`):**
```bash
python3 -c "import json;d=json.load(open('.flowcast/runs/<run-id>/state.json'));print(d['status'],d.get('currentStep'),d.get('verdict','-'),d.get('failedStep','-'));print('done:',[s['key'] for s in d['steps'] if s['status']=='done'])"
```

**Liveness FIRST (the #1 failure mode):** a dead flow looks identical to an
idle flow (no new log bytes). Before ever saying "healthy, no intervention",
confirm the flow is actually alive:
```bash
tmux ls | grep recursive-flow   # session present?
pgrep -fl "self-improve.flow.js" # node process alive?
```
If the tmux session is gone **and** `state.json` still says `running` **and**
no terminal event (`fatal`/`verdict`/non-zero exit) was emitted → **the flow
crashed.** Do not narrate "no intervention needed" over a corpse. Read the log
tail for the stack trace and intervene (see step 5).

**Log path gotcha (bit me once):** the launcher generates a **new timestamped
log file per launch**. When you resume/re-launch, the log path changes.
Derive the current log from the run dir's `events.jsonl` mtime or glob the
newest `.flowcast/logs/flow-*.log` — do not assume last run's path still
applies.

### 5. Intervene only when it can't self-heal

- **Crash** (process gone, no verdict) → read log tail, diagnose, apply the
  **minimal** fix (PATH, dep, config), then **resume** with the same run-id:
  `launch-flow.sh --run-id <id> --goal-file <path> --provider ...`. Note: a
  resume **still requires `--goal-file`/`--goal`** — the run-id alone errors
  `缺少 --goal 或 --goal-file`. (The run-id reuses the run *dir* for outputs;
  it does not restore the goal text.)
- **Transient gate red that the agent is already fixing** (e.g. e2e
  `SESSION_EXISTS`) → **do nothing**. The resume-fix loop handles it. Only
  step in if `MAX_FIX_ROUNDS` is exhausted and it goes `failed-preserved`.
- **Missing prerequisite the agent can't install** → fix it (start Docker,
  install a CLI), then resume.
- **Decision only a human can make** → stop polling, ask the user crisply,
  don't arm another sleep.
- **Diagnosing a flow-script bug** (rare): `withSelfModGuard` refuses a dirty
  `.dev/`, so you must commit any diagnostic patch first, then `git revert`
  it after. Prefer a standalone reproducer script in `/tmp` (CJS `node -e` or
  a `.js` file) over patching `.dev/flows/*.js` — it keeps main's history
  clean. (Real run: two throwaway `wip(diag)` commits polluted main before the
  real goal commit; avoidable.)

### 6. Reach verdict, verify, clean up

When `status: completed`:
- Read `report.md` for `verdict`. If `committed`, confirm it actually landed:
  `git -C recursive merge-base --is-ancestor <commit> main && echo ON_MAIN`.
- Verify the product independently: run the relevant test subset yourself
  (e.g. `cargo test -p recursive-tui`), don't just trust the green gate.
- Clean up: `git -C recursive worktree remove .worktrees/<run-id> --force`
  (the flow usually does this, but confirm), `tmux kill-session` for any
  leftover, remove `/tmp` scratch files.
- If you left any diagnostic commits on main, `git revert` them (don't reset —
  the goal commit sits on top; revert keeps history honest).

## Discipline (the lessons, condensed)

1. **Never pre-compile before launch.** Contention on `target/` produces
   phantom `preflight.build` failures. The flow builds for you.
2. **Liveness before "healthy".** A crashed flow emits nothing; checking only
   `state.json == running` will have you narrate progress over a corpse.
3. **Capture run-id + tmux + log path at launch.** Re-derive the log path on
   resume; don't trust the previous path.
4. **Resume needs `--goal-file` again.** `--run-id` alone is insufficient.
5. **Don't intervene on transient gate reds.** Know the `onFail` policy;
   `resume-fix` means the agent is already on it. Watch `MAX_FIX_ROUNDS` (=3).
6. **Prefer /tmp reproducers over patching `.dev/flows/*.js`.** If you must
   patch the flow script to diagnose, commit→run→`git revert`; never leave
   `wip(diag)` commits on main.
7. **Verify the product yourself.** Green gates ≠ correct code. Re-run the
   headline test after landing.
8. **All git in the sub-repo.** This is a recursive change; the monorepo root
   only owns `mona.yaml` + docs.

## Quick reference

```bash
# launch
cd recursive && .dev/scripts/launch-flow.sh --goal-file .dev/goals/NN-*.md --provider deepseek --hitl wecom

# one tick (sleep + probe)
sleep 180 && python3 -c "import json;d=json.load(open('.flowcast/runs/<RID>/state.json'));print(d['status'],d.get('currentStep'),d.get('verdict','-'),d.get('failedStep','-'));print([s['key'] for s in d['steps'] if s['status']=='done'])"

# liveness
tmux ls | grep recursive-flow; pgrep -fl self-improve.flow.js

# resume after crash/intervention (goal-file REQUIRED)
.dev/scripts/launch-flow.sh --run-id <RID> --goal-file .dev/goals/NN-*.md --provider deepseek --hitl wecom

# verify landing
git -C recursive merge-base --is-ancestor <COMMIT> main && echo ON_MAIN
cargo test -p recursive-tui   # or the relevant crate

# list historical runs
node .dev/flows/self-improve.flow.js --list
```

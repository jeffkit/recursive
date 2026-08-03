---
type: Skill
name: self-improve-supervise
description: "ZCode-as-supervisor playbook for running Recursive's self-improve flow (.dev/flows/self-improve.flow.js). Location-independent: run from the infra4agent monorepo root OR the recursive repo itself — step 0a resolves the recursive root into $RR. Use when the user wants YOU (not recursive's own agent) to drive a self-improve goal end-to-end: write the goal, launch the flow, monitor it, intervene only when it can't self-heal, and report the verdict. Event-driven: arm a Bash watcher with run_in_background: true; the kernel wakes you with a <task-notification> when the flow reaches verdict or dies. Foreground sleep+probe is only a forensic fallback for between-tick steering. (This contrasts with recursive's *own* loop-supervise skill, which targets recursive's kernel and uses its run_background/watch_file/schedule_wakeup/stop_loop toolset.)"
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

> **ZCode IS event-driven.** Arm a single `Bash` call with
> `run_in_background: true` that polls `state.json` until the flow reaches
> `verdict` or the node process dies; the kernel wakes you with a
> `<task-notification>` when the watcher exits. That's the default — one
> tool call to arm, one to handle the verdict. Foreground `sleep` + probe
> is a **forensic fallback** for cases where you need to steer between
> ticks (e.g. read a mid-run diff to decide whether to intervene before
> the next state change).
>
> This skill is the **ZCode** version. Recursive ships its own
> `loop-supervise` skill for recursive's *own* agent kernel — that targets a
> different toolset (`run_background` returning a job_id, `watch_file`,
> `schedule_wakeup`, `stop_loop`). Don't conflate the two: when this skill
> says "watcher", it means a
> backgrounded `Bash` invocation, not a recursive-kernel primitive.

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
| Wait + watch to verdict | `Bash` with `run_in_background: true` (arm a polling watcher) | **DEFAULT.** The watcher polls `state.json` and exits on terminal event; ZCode wakes you with `<task-notification>`. This IS the heartbeat. |
| Read progress | `Read` / `Bash` (`cat`/`python3 -m json.tool`) on `state.json`, `run.log.jsonl`, the log file | pick paths from `run-id`, don't hardcode |
| Inspect the agent's work mid-run | `Bash` `git -C <worktree> diff` | read-only on the worktree is safe |
| Kill a stuck/hung flow | `Bash` `tmux kill-session -t <name>` | `tmux ls` to find the name |
| Foreground sleep+probe (forensic only) | `Bash` with `sleep N` | use only when you need to steer between ticks (e.g. read a mid-run diff and decide before next state change). One tool call per tick. |

**What you have for event-driven wake:** `Bash` with
`run_in_background: true` — the watcher exits, ZCode delivers a
`<task-notification>` with `status: completed` (or `failed`) + exit code +
stdout path. **What ZCode does not have:** a cron-style mid-run scheduler
(`CronCreate` exists but is for periodic triggers, not for waking you
mid-run). Don't write SOP text that calls for `watch_file`,
`schedule_wakeup`, `stop_loop`, `tool_search`, or `run_background` returning
a job-id — those are **recursive's kernel** primitives, not ZCode's.

## SOP

### 0a. Resolve the recursive root (`$RR`) — do this first, every session

This skill works whether ZCode's cwd is the **infra4agent monorepo root**
(recursive lives under `recursive/`) or the **recursive repo itself** (`.`).
Resolve `$RR` once and use it in every later command — never hardcode
`recursive/` or `cd recursive`:

```bash
if   [ -f .dev/flows/self-improve.flow.js ];        then RR=.          # cwd IS recursive
elif [ -f recursive/.dev/flows/self-improve.flow.js ]; then RR=recursive # cwd is monorepo root
else echo "ERROR: run from the monorepo root or the recursive repo" >&2; exit 1; fi
echo "recursive root -> $RR"
```

From here on, `$RR` is the recursive repo root: `$RR/.dev/...`,
`$RR/.flowcast/...`, `git -C "$RR" ...`, `(cd "$RR" && .dev/scripts/...)`.
All git targets the `recursive` sub-repo, never the infra4agent monorepo root.

### 0b. Read the contract (before writing the goal)

- Read `$RR/AGENTS.md` + `$RR/.dev/AGENTS.md` for source invariants (the
  kernel must stay small; no `unwrap()`; finish reasons are data; etc.). A
  goal that violates an invariant will burn fix-rounds.
- Skim `mona.yaml` / `docs/ARCHITECTURE.md` at the monorepo root only if the
  goal crosses sub-repos (it usually shouldn't for a recursive self-improve).

### 1. Write the goal file

- Next number = `max(ls "$RR/.dev/goals/" | grep -oE '^[0-9]+') + 1`. Mirror
  the format of a recent same-flavour goal (TUI goals look different from
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
git -C "$RR" add .dev/goals/NN-*.md
git -C "$RR" commit -m "dev: add goal NN — <one line>"
```

`launch-flow.sh` **refuses to start if the worktree is dirty**
(`withSelfModGuard`). The goal file must be committed first. Verify:
`git -C "$RR" status --porcelain | wc -l` → must be `0`.

### 3. Launch (and capture the run-id)

```bash
cd "$RR"
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

### 4. Monitor — arm a background watcher (event-driven)

**Default path:** one `Bash` call with `run_in_background: true` that
sleeps + probes `state.json` in a loop, exits when the flow reaches a
terminal state (`status: completed`, `verdict: ...`) or when the node
process dies. The kernel wakes you with a `<task-notification>` at exit —
you spend one tool call to arm it and one to handle the verdict. The
template lives in the Quick reference (`# Background-poll to terminal`).

**Foreground `sleep` + probe is the forensic fallback.** Reach for it
when you need to *steer between ticks* (e.g. the watcher noticed a state
change you want to inspect before the next tick: read the agent's diff,
decide whether to intervene). Each tick is one `Bash` call that sleeps
**then** reads state.

**Tick cadence (for either path):** preflight is fast (~30–90s) → sleep
60–90s. `run.recursive` is the long phase (10–25 min for a real change)
→ sleep 150–240s. Gate phase is bursty → sleep 60–120s. Lean longer when
stable; shorter after a state change.

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
  **minimal** fix (PATH, dep, config), then **resume** from within `$RR`:
  `.dev/scripts/launch-flow.sh --run-id <id> --goal-file <path> --provider ...`.
  Note: a resume **still requires `--goal-file`/`--goal`** — the run-id alone
  errors `缺少 --goal 或 --goal-file`. (The run-id reuses the run *dir* for
  outputs; it does not restore the goal text.)
- **Transient gate red that the agent is already fixing** (e.g. e2e
  `SESSION_EXISTS`, or `gate.e2e` timing out because the docker build alone ate
  the whole timeout budget) → **do nothing**. The resume-fix loop handles it:
  `gate.e2e.fix-1` / `.fix-2` in the `done` list is the agent re-running the
  gate (image now cached → fast). Only step in if `MAX_FIX_ROUNDS` (=3) is
  exhausted and it goes `failed-preserved`. **`gate.e2e.fix-1` is normal** for
  any goal touching `src/` — expect it, don't panic, don't intervene.
- **`verdict: failed-preserved` but the work looks correct** → the watchdog
  may have mis-killed the agent during a synchronous finishing move (writing
  the journal, running a final `cargo test`) that produced zero transcript
  growth and had no descendant processes. The agent's complete, self-verified
  edit is NOT lost: it sits in `refs/preserve/<run-id>` + a
  `preserve: <reason>` commit in the worktree. See step 6's rescue procedure.
  Known mis-fire trigger: the `no-growth-hung` watchdog (g353, g349 lesson).
  (A winddown-recognition fix landed in the watchdog, but verify the run's
  failure log before assuming rescue is needed.)
- **Missing prerequisite the agent can't install** → fix it (start Docker,
  install a CLI), then resume.
- **Decision only a human can make** → stop the watcher, ask the user
  crisply, don't arm another background poll.
- **Diagnosing a flow-script bug** (rare): `withSelfModGuard` refuses a dirty
  `.dev/`, so you must commit any diagnostic patch first, then `git revert`
  it after. Prefer a standalone reproducer script in `/tmp` (CJS `node -e` or
  a `.js` file) over patching `.dev/flows/*.js` — it keeps main's history
  clean. (Real run: two throwaway `wip(diag)` commits polluted main before the
  real goal commit; avoidable.)

### 6. Reach verdict, verify, clean up

When `status: completed`:
- Read `$RR/.flowcast/runs/<run-id>/report.md` for `verdict`. If `committed`,
  confirm it actually landed:
  `git -C "$RR" merge-base --is-ancestor <commit> main && echo ON_MAIN`.
- **If `failed-preserved`**: read `report.md`'s `detail` + `watchdog-failure.log`
  + `preserved.diff` + the `refs/preserve/<run-id>` commit. The verdict means a
  gate/review loop exhausted OR the watchdog mis-killed; it does NOT mean the
  code is wrong. Decide:
  - **Watchdog mis-fire, code looks correct** (the common rescue): the agent's
    complete, self-verified edit is in the preserve commit. `cherry-pick` it
    onto main, then **independently run the three gates yourself**
    (`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
    --all-features -- -D warnings`, `cargo test --workspace`). If green, amend
    the cherry-picked commit's message (it'll have the placeholder
    `preserve: <reason>` message) to a proper `feat/fix(...): Goal NNN — …`
    message and keep it. If red, `git reset --hard ORIG_HEAD` and report.
    Commands:
    ```bash
    PRESERVE_COMMIT=$(git -C "$RR" log --format=%H refs/preserve/<run-id> -1)
    git -C "$RR" cherry-pick "$PRESERVE_COMMIT"
    # re-run gates; if green:
    git -C "$RR" commit --amend -m "feat(...): Goal NNN — <one line>…"
    ```
  - **Genuinely broken gate/review after MAX_FIX_ROUNDS**: leave it preserved,
    read the failure context, write a follow-up goal. Do NOT cherry-pick code
    that failed its own gates.
- Verify the product independently: run the relevant test subset yourself
  (e.g. `cargo test --manifest-path "$RR/Cargo.toml" -p recursive-tui`), don't
  just trust the green gate. **Specifically confirm the goal's headline test
  exists and passes by name** — `cargo test <name-substring>`. A green
  workspace doesn't prove the agent added the test the goal required.
- Clean up: `git -C "$RR" worktree remove .worktrees/<run-id> --force` (the
  flow usually does this for `committed`; for `failed-preserved` the worktree
  is at `.worktrees/preserve/<run-id>` and you must remove it yourself),
  `tmux kill-session` for any leftover, remove `/tmp` scratch files.
- If you left any diagnostic commits on main, `git revert` them (don't reset —
  the goal commit sits on top; revert keeps history honest).

## Discipline (the lessons, condensed)

1. **Never pre-compile before launch.** Contention on `target/` produces
   phantom `preflight.build` failures. The flow builds for you. **If you see
   `error: invalid character ' ' in package name: ' recursive-cli'`** (note the
   leading space), the cause is NOT a corrupted target/ lock — it's a bug in
   `.dev/flows/self-improve.flow.js` itself: an argv element like
   `'-p recursive-cli'` written as ONE string (space inside the quotes) instead
   of two `'-p', 'recursive-cli'`. cargo then parses the `-p` value as
   ` recursive-cli` (leading space) → invalid-char. Diagnose by wrapping cargo
   with a `/tmp` shell script that logs `argv`, OR check the flow source:
   `grep -n "'-p [^']*'" .dev/flows/self-improve.flow.js` (a `-p X` with the
   space INSIDE the quotes is the bug). The fix is a one-char comma insertion.
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
9. **`gate.e2e` is the cost center.** Any goal touching `src/` or
   `crates/*/src/` pays 25-40 min of docker build (colima compiles the whole
   recursive + AWS SDK inside the image). An e2e-gate diff-scope short-circuit
   skips it for docs/tests/`.dev/`-only changes (verified: a tests-only goal's
   `gate.e2e` ran in 435ms vs ~25min). So: **if you have a choice, batch the
   pure-test/docs goals** — they skip e2e and finish in minutes. When
   monitoring an e2e-bearing goal, arm 2-3 min sleeps (not 90s, not 5 min) —
   with cargo-chef caching (see below) the docker build is ~1min for src/
   changes, ~10min only for the first build after a Dockerfile/Cargo.lock change.
   **cargo-chef 3-stage Dockerfile** (`e2e/Dockerfile`) makes src/ changes
   cheap: planner generates recipe.json (no src needed), cooker compiles all
   588 deps from recipe.json (CACHED across worktrees because BuildKit layer
   cache keys on file CONTENT hash, not context path), builder compiles only
   the 3 workspace crates. So a src/ change costs **~1min docker build in ANY
   worktree**, not ~12min. The first build after a Dockerfile change or
   Cargo.lock change pays a ~10min cooker rebuild (one-time). Do NOT re-add
   `COPY src` to the planner stage — it invalidates the cooker cache and
   reverts worktree builds to ~12min each (hard-won lesson).
10. **`failed-preserved` ≠ broken.** The watchdog (`no-growth-hung`) can
    mis-kill an agent that's writing its journal or running a final `cargo
    test` — synchronous moves with zero transcript growth and no child
    processes. The complete, self-verified edit survives in
    `refs/preserve/<run-id>` + a `preserve: <reason>` commit. Rescue it by
    cherry-pick + independent gate re-run (see step 6). Don't re-run the goal
    from scratch — that burns another 25 min and may hit the same watchdog.
11. **e2e first-red is normal; `gate.e2e.fix-1` is the agent self-healing.**
    e2e often times out on the first run because the docker build eats the
    whole gate timeout; on `fix-1` the image is cached and it passes. Seeing
    `gate.e2e.fix-1` in the done list is GOOD news, not a problem. Only
    intervene if it goes `.fix-2`, `.fix-3`, then `failed-preserved`.
12. **The dirty-tree guard has TWO layers.** `launch-flow.sh` checks
    `git status --porcelain`, AND flowcast's own `withSelfModGuard`
    (`node_modules/flowcast/self-mod-guard.js`, NOT in our repo) re-checks at
    `preflight.baseline`. Loosening launch-flow.sh alone is useless — the
    inner guard still rejects. **Keep the tree clean before every launch:**
    commit ALL goal drafts, even untracked ones. There's no shortcut.
13. **Background-poll to terminal, don't babysit.** Arm ONE
    `run_in_background: true` poll loop per goal that sleeps + checks
    `state.json` until `verdict` appears or the node process dies, then
    notifies you. You spend one tool call to arm it and get woken at the end —
    far cheaper than 10+ foreground sleep-probe cycles. Template:
    `for i in $(seq 1 30); do sleep 90; pgrep -f self-improve.flow.js || break;
    <read verdict>; <break if terminal>; done` with `run_in_background`.
14. **Verify the headline test by NAME, not just "test suite green".** After
    landing, run `cargo test <goal-name-substring>` (e.g.
    `cargo test cancelled_mid_stream`) and confirm the specific tests the goal
    required exist and pass. A green workspace can hide a missing test.
15. **Goal-writing: be unambiguous and prescriptive.** The agent only has the
    goal text. A good goal has: **Design principle check** (state it does NOT
    violate invariant #1), **Why** (root cause with file:line), **Scope** (do
    EXACTLY this — numbered steps with code snippets), **Files NOT to touch**
    (explicit allowlist of what's out of scope), **Acceptance** (exact gate
    commands + grep-verifiable checks), **Notes for the agent** (traps:
    invariants, API signatures to verify, ordering constraints). Mirror a
    recent same-flavour goal's format. Ambiguity costs fix-rounds.
16. **Sequence goals by leverage, and mix scopes.** Order: real bugs >
    invariant/gate holes > test gaps > cleanup. **Deliberately mix src/ and
    tests-only goals** in a batch — the tests-only ones skip e2e (fast,
    ~15min) and let you keep momentum while an e2e-heavy goal churns.
17. **When in doubt about a fix's scope, prefer /tmp reproducers.** Before
    patching `.dev/flows/*.js` or `.dev/scripts/*.sh` to diagnose, write a
    standalone reproducer (`node -e`, a bash wrapper that logs argv, a small
    `.rs` file). It keeps main clean and often pinpoints the bug faster than
    reading the full script. (Real run: a cargo wrapper in `/tmp/cargo-diag/`
    captured the `argv: ['-p recursive-cli']` single-element bug in one shot.)
18. **Push only when the user asks.** Self-improve commits land on the local
    `recursive` sub-repo's main. Do NOT `git push` unless explicitly told —
    "land" and "publish" are different actions. Confirm the remote state first
    (`git fetch; git status -sb`) and note any pre-receive-hook quirks (an old
    issue doc in `.dev/issues/` may be stale; a clean push in 2026-08
    succeeded despite the doc claiming otherwise).
19. **A gate that runs in ~100ms with "syntax error" in its output is NOT
    passing — it's broken.** Every mutants gate in this codebase was silently
    false-green for an unknown period: `gates.json` invoked bash-only scripts
    (`< <(...)` process substitution) via `sh`, which aborts at parse time;
    the `trap cleanup_mutants EXIT` then re-emitted `$?` captured *inside the
    trap* (0 = trap's own success), masking the abort as `passed: true`. The
    symptom — `gate.*-mutants` completing in ~100ms across every goal — was
    visible in `run.log.jsonl` the whole time but read as "fast增量变异".
    **Action:** when any gate shows suspiciously fast completion (<1s for
    something that compiles), grep its `result.output` in `run.log.jsonl` for
    `syntax error` / `command not found` / `unexpected token`. A real
    cargo-mutants run takes minutes, not milliseconds. (Incident: cycle
    2026-08-02, all goals 370-375; fixed in commit `17fb4da` — `sh`→`bash` in
    gates.json + `< <(...)`→temp-file in the scripts + documented the `$rc`
    capture trap.)
20. **cargo-mutants contamination survives cherry-pick.** When you (or the
    agent) run `cargo-mutants --in-place` in a worktree and the run is
    interrupted (Ctrl-C, kill, watchdog), the mutated source lines carrying
    `/* ~ changed by cargo-mutants ~ */` are NOT auto-restored. If you then
    `git cherry-pick` or `git diff` from that worktree, the contamination
    lands on main as a real code change. **Two real incidents this cycle:**
    (a) supervisor's own verification run left `||`→`>` in `truncate_str`
    (`src/lib.rs`); (b) the agent's self-check left `||`→`&&` in
    `OldPermissionsConfig::From` (`src/permissions/mod.rs`) — the latter was
    caught by `test_old_config_only_deny_produces_layer`, the former by a
    manual `git diff` scan. **Action:** before ANY cherry-pick from a worktree
    where cargo-mutants ran, `grep -rln "changed by cargo-mutants" src/
    crates/` and `git checkout --` every hit. Never trust a worktree that has
    a `mutants.out/` dir without scanning first.
21. **Self-improve goals change the AGENT's source, not the dev scaffolding.**
    The flow's `run.recursive` agent may only edit `src/`, `crates/*/src/`,
    `Cargo.toml`, `tests/`, `e2e/` — i.e. things the shipped agent depends on.
    `.dev/scripts/*.sh`, `.dev/flows/*.js`, `.flowcast/gates.json`, and the
    skill files themselves are **supervisor/infrastructure** — editing them via
    a self-improve goal is "the examinee rewriting the exam." If a cycle finds
    a bug in the scaffolding (e.g. the mutants-gate false-green), the
    supervisor fixes it with a direct commit, NOT a goal. When writing goals,
    self-check the scope: if a proposed change touches `.dev/` or
    `.flowcast/`, it's out of bounds — record it as a follow-up for the human
    instead. (Incident: cycle 2026-08-02, a proposed "goal 376" to fix
    `e2e-gate.sh` diff-scope was correctly rejected as out-of-scope after the
    user flagged the boundary.)
22. **Stop an agent that over-spends its budget self-checking.** Goal 373's
    agent completed all code changes correctly, then spent 50+ minutes (and
    ~120 steps) running a full 4994-mutant `cargo-mutants` self-check inside
    `run.recursive`, polling with `sleep 280` (zero transcript growth →
    watchdog-bait). It would have either hit the step cap
    (`failed-preserved`) or been `no-growth-hung` killed — both wasted.
    **Action:** when `run.recursive` runs >2× longer than同类 goals AND the
    log shows the agent polling a long background job (`cargo mutants`,
    `cargo test --all`, a big build) instead of editing, it has finished
    writing and is over-verifying. Read the worktree diff: if the changes look
    complete, stop the flow (`tmux kill-session` + `pkill`), cherry-pick the
    worktree, and verify yourself. The gate phase exists to run the expensive
    checks — the agent shouldn't duplicate them in `run.recursive`. (Incident:
    goal 373, cycle 2026-08-02; rescued as `82d7ccb`.)

## Quick reference

```bash
# resolve recursive root first (RR=. if cwd IS recursive, RR=recursive if cwd is monorepo root)
if   [ -f .dev/flows/self-improve.flow.js ];        then RR=.;
elif [ -f recursive/.dev/flows/self-improve.flow.js ]; then RR=recursive; fi
export RR

# launch
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/NN-*.md --provider deepseek --hitl wecom)

# one tick (sleep + probe) — paths under $RR
sleep 180 && python3 -c "import json;d=json.load(open('$RR/.flowcast/runs/<RID>/state.json'));print(d['status'],d.get('currentStep'),d.get('verdict','-'),d.get('failedStep','-'));print([s['key'] for s in d['steps'] if s['status']=='done'])"

# liveness
tmux ls | grep recursive-flow; pgrep -fl self-improve.flow.js

# resume after crash/intervention (goal-file REQUIRED) — run from within $RR
(cd "$RR" && .dev/scripts/launch-flow.sh --run-id <RID> --goal-file .dev/goals/NN-*.md --provider deepseek --hitl wecom)

# verify landing
git -C "$RR" merge-base --is-ancestor <COMMIT> main && echo ON_MAIN
cargo test --manifest-path "$RR/Cargo.toml" -p recursive-tui   # or the relevant crate

# background-poll to terminal (arm with run_in_background: true; get notified at verdict)
# 60 iters × 90s = 90min ceiling — covers goals whose agent self-checks long (e.g. cargo-mutants)
RID=selfimprove-XXXXXXXXX
for i in $(seq 1 60); do
  sleep 90
  pgrep -f self-improve.flow.js >/dev/null 2>&1 || { echo "node gone"; break; }
  read st v step <<< $(python3 -c "import json;d=json.load(open('$RR/.flowcast/runs/$RID/state.json'));print(d.get('status'),d.get('verdict') or '-',d.get('currentStep') or '-')")
  [ "$st" = completed ] || [ "$v" != - ] && { echo "TERMINAL: $st $v $step"; break; }
  echo "tick $i: $step"
done

# rescue a failed-preserved run whose work is correct (watchdog mis-fire case)
PRESERVE_COMMIT=$(git -C "$RR" log --format=%H refs/preserve/<RID> -1)
git -C "$RR" cherry-pick "$PRESERVE_COMMIT"
# CRITICAL (lesson 20): if the worktree ever ran cargo-mutants, contamination may
# have landed with the cherry-pick. Scan + clean BEFORE running gates:
grep -rln "changed by cargo-mutants" "$RR/src/" "$RR/crates/" 2>/dev/null | xargs -r git -C "$RR" checkout --
# then independently re-run the 3 gates; if green, amend the message:
git -C "$RR" commit --amend -m "feat(...): Goal NNN — <one line>"
# if red: git -C "$RR" reset --hard ORIG_HEAD

# verify the goal's headline test BY NAME (not just "test suite green")
cargo test --manifest-path "$RR/Cargo.toml" <name-substring>   # e.g. cancelled_mid_stream

# spot a silently-broken gate (lesson 19): ~100ms + "syntax error" in output = broken, not passing
python3 -c "
import json
for line in open('$RR/.flowcast/runs/<RID>/run.log.jsonl'):
    d=json.loads(line)
    if d.get('event')=='done' and 'mutant' in d.get('key',''):
        r=d.get('result',{}); out=str(r.get('output',''))[:80]
        print(d['key'],'passed='+str(r.get('passed')),'dur='+str(d.get('durationMs'))+'ms',out)
"

# diagnose a phantom preflight.build error (invalid char in package name)
grep -n "'-p [^']*'" "$RR/.dev/flows/self-improve.flow.js"   # a -p X with space INSIDE quotes = bug

# list historical runs
node "$RR/.dev/flows/self-improve.flow.js" --list
```

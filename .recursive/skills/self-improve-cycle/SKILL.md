---
type: Skill
name: self-improve-cycle
description: "Drive a full self-improve cycle with Recursive's own tools: architect review (parallel Explore sub-agents via the agent tool) → generate goals → iterate them through the self-improve flow (launch-flow.sh, supervised with loop-supervise) → verify + report. Orchestration layer on top of loop-supervise (which handles a single goal). Use when the user wants a periodic/scheduled quality sweep of the recursive codebase, or says '找问题跑自迭代' / 'do a review cycle'. Supports concurrent execution of independent goals across worktrees, and auto-distills lessons learned back into skills."
mode: trigger
triggers: self-improve-cycle, review cycle, 审查周期, 找问题跑自迭代, 定期扫描, quality sweep
---

# self-improve-cycle — Orchestrate a full review→goal→iterate cycle

## What this skill is

The **orchestration layer** on top of
[`loop-supervise`](./loop-supervise/SKILL.md) (which handles a single goal
end-to-end). This skill drives the **whole cycle**:

```
architect review (parallel Explore sub-agents) → prioritize findings → write goals →
iterate (concurrent if independent) → verify each → distill lessons → report
```

**Load `loop-supervise` first** — this skill assumes you know its SOP
(`run_background` + `watch_file` + `schedule_wakeup` monitor pattern, verdict
semantics, rescue procedures, and the discipline lessons). This is the
**recursive** version: the ZCode counterpart (`.zcode/skills/self-improve-cycle/`)
targets ZCode's toolset (Bash `run_in_background: true` + `<task-notification>`);
here, "sub-agents" means the `agent` tool with `mode: "parallel"` and the
built-in `explore` role, and "watcher" means the loop-supervise primitives.

## When to use

- The user wants a **quality sweep**: "找问题跑自迭代", "do a review cycle",
  "定期扫描", "看看有什么能改进的"
- **Periodic/scheduled** maintenance (weekly codebase health check)
- After a **major change batch** (verify nothing regressed, find follow-ups)

## When NOT to use

- Single goal execution — use `loop-supervise` directly.
- Pure exploration / "explain how X works" — no goal to iterate.
- The user has a **specific** goal already — just supervise it, don't review.

---
## Scope boundary — review the AGENT's Rust source only

A self-improve cycle reviews and improves the **agent program itself** — the
Rust code under `src/`, `crates/*/src/`, plus `Cargo.toml`, `tests/`, `e2e/`.
It does NOT review or modify the development tooling around the cycle:
`.dev/scripts/*.sh`, `.dev/flows/*.js`, `.flowcast/*`, and the skill files
(`.recursive/skills/*`) are **supervisor infrastructure** — "the examinee
rewriting the exam" (loop-supervise → Supervisor discipline). Scaffolding
findings are fixed with a **direct commit**, never a goal.

## SOP

### Phase 1 — Plan the review (align before diving)

1. **Load `loop-supervise`** (its SOP + discipline lessons).
2. **Resolve the repo root.** If cwd is the recursive repo: `RR=.`. If the
   infra4agent monorepo root: `RR=recursive`. All git/launch commands target
   `$RR`.
3. **Read `$RR/AGENTS.md` + `$RR/.dev/AGENTS.md`** for current invariants +
   contracts.
4. **Check what's changed since last review**: scan recent `git -C $RR log`
   + the journal dir (`$RR/.dev/journal/`) for hints about what's fresh or
   fragile.
5. **Pick 3-4 review angles** (see the angle bank below). Tell the user:
   > "I'll review from these angles: A, B, C, D. Expecting ~N goals. Confirm?"

   **Wait for confirmation** before diving — don't burn sub-agent budget on
   angles the user doesn't care about.

### Phase 2 — Review (parallel sub-agents)

Launch **3-4 Explore sub-agents in parallel** with the `agent` tool:
`mode: "parallel"`, one manifest entry per angle, each using the built-in
`explore` role (`.recursive/agents/explore.md` — read-only search specialist).
Each sub-agent prompt:

- State the scope (the angle) + tell it to read `.dev/AGENTS.md` invariants first.
- Demand: ranked P0→P3 findings, each with **file:line evidence** + **one-line
  fix direction**. Conclusions only, not file dumps.
- Demand: distinguish "real bug / panic / data loss" from "nice-to-have".
- Done-criterion: report paths + line numbers + signatures; **do not modify
  files** (the explore role is read-only by construction).

**When sub-agents return**: don't blindly trust — **personally verify the top
2-3 findings** (read the cited code, confirm the bug is real). Sub-agents can
hallucinate file:line or misread intent. Your verification is the gate.

### Phase 3 — Generate goals

Turn verified findings into goal files (`$RR/.dev/goals/NNN-*.md`). Rules:

- **One finding = one goal** (don't bundle unrelated fixes — they may fail
  independently and force a preserve).
- **Mirror a recent same-flavour goal's format** (read one neighbour).
- **Each goal MUST have**: Design principle check, Why (root cause + file:line),
  Scope (numbered steps + code), Files NOT to touch, Acceptance (grep-verifiable
  + gate commands), Notes for the agent (traps).
- **Next goal number** = `max(ls $RR/.dev/goals/ | grep -oE '^[0-9]+') + 1`.
- **Commit each goal file** before launching (clean-tree mandate — the
  dirty-tree guard has TWO layers, see loop-supervise trap 4).

**Prioritize**: real bugs (panic/crash/data-loss) > invariant/gate holes >
test gaps > docs/cleanup. Tell the user the proposed ordering + scope mix,
confirm, then proceed.

### Phase 4 — Iterate (sequential or concurrent)

#### Sequential mode (default, safest)

Run goals one at a time: launch → supervise to verdict → verify → clean up →
launch next. This is the simple, safe default. Use it when:
- goals may have hidden dependencies (touch overlapping code)
- this is your first cycle (build intuition)
- concurrency is risky (see below)

#### Concurrent mode (optional, for independent goals)

If you have **N independent goals** (touch disjoint files, no shared API
contract), you MAY launch them concurrently — each in its own worktree +
tmux session. This saves wall-clock time when you have several goals.

**Concurrency safety rules — read carefully:**

1. **Independence check**: two goals are concurrent-safe only if they touch
   **disjoint files** AND don't change a **shared contract** (e.g. a trait
   signature, a public API, an error enum variant). If goal A adds an
   `Error::Foo` variant and goal B matches on `Error` exhaustively, they
   conflict even if files differ — run sequentially.
2. **e2e gate isolation**: argusai (v0.14.3+) namespaces container names
   (`<namespace>-recursive-e2e` + `--network-alias` preserving in-network DNS)
   and uses random host ports (`ports: ["0:8080"]`). **Multiple concurrent e2e
   gates no longer collide.** So `src/` goals CAN run concurrently — the only
   remaining constraint is independence (rule 1) + LLM rate limits (rule 3).
3. **LLM rate limits**: each goal's agent + reviewer makes many LLM calls.
   2-3 concurrent goals is usually fine; >3 may hit rate limits.
4. **Monitor each independently**: one supervised loop per concurrent goal
   (each has its own tmux session; a crash in one doesn't affect the other).
   With recursive's tools this means one `run_background`-based watcher
   (`watch_file` on each run's events / `schedule_wakeup` heartbeat) per goal —
   keep the run-ids straight, poll each on its own cadence.

**Recommended concurrency pattern** (safe default):
- **2-3 independent goals** concurrently — regardless of whether they're
  `src/` or tests/docs (e2e isolation now handles concurrent docker builds).
- When the batch finishes, launch the next batch.
- The binding constraint is now LLM rate limits + goal independence, NOT
  e2e container collision.

#### Per-goal workflow (loop-supervise SOP)

For each goal, concurrent or not:
- Launch via `launch-flow.sh` (capture run-id + tmux + log path).
- Supervise to terminal (loop-supervise: probe liveness FIRST, then
  `state.json` tick).
- On `committed`: verify product independently (run the headline test by name).
- On `failed-preserved`: check if it's a watchdog mis-fire → cherry-pick
  rescue (see loop-supervise "Verdict handling & rescue"); scan for
  cargo-mutants contamination first. If genuinely broken, report + skip.
- Clean up worktree + tmux after each.

### Phase 5 — Distill lessons (CRITICAL — do not skip)

**After each cycle (or after a notable incident mid-cycle), update the skills
with what you learned.** This is how the next session's supervisor gets
smarter — it's the compounding mechanism.

**What to distill:**
- A new failure mode you hadn't seen (e.g. "watchdog mis-killed during X")
- A flow/script bug you diagnosed (e.g. "argv bug in preflight.build")
- A performance insight (e.g. "cargo-chef cooker cache doesn't share across
  worktrees if planner COPYs src")
- A reusable diagnostic technique (e.g. "wrap cargo with a /tmp logger to
  capture real argv")
- A goal-writing lesson (e.g. "ambiguous scope burns fix-rounds")

**Where to distill:**
- **`loop-supervise`** (`.recursive/skills/loop-supervise/SKILL.md`) → Gate
  semantics / Verdict handling & rescue / Supervisor discipline sections
  (add a lesson: trigger + symptom + action + incident).
- **This skill** → if it's about the cycle-level orchestration (review angles,
  concurrency strategy, prioritization).
- **Note**: `.recursive/skills/` is protected from the Edit/Write tools
  (SafetyCheck — prevents a running agent from tampering with its own skills).
  Edit these files via Bash (write to `/tmp`, then `cp`), and commit the change
  as a manual journal entry (`.dev/journal/manual-<YYYYMMDD>-<tag>.md`).

**How to distill (quality bar):**
- Write the **trigger** (when you'll hit this again) + the **symptom** (what
  you observe) + the **fix/action** (what to do). Not vague aphorisms.
- Cite the **incident** that produced it (goal number, commit, what happened).
  Future readers can trace it.
- Keep it **actionable** — "if you see X, do Y", not "be careful with X".

**Self-check before finishing the cycle**: "Did anything unexpected happen
that the current skills don't cover?" If yes → add a lesson. If the cycle was
entirely smooth → probably nothing new to distill (that's fine, don't
fabricate).

### Phase 6 — Report

Summarize for the user:
- How many goals, verdicts (committed/failed-preserved/skipped), commits.
- Key findings (the most impactful bugs/fixes).
- Any lessons distilled (what was added to skills).
- Suggested follow-ups (findings deferred, performance items for human).
- **Do NOT push unless the user explicitly asks.**

---

## Review angle bank

Pick 3-4 per cycle. Rotate across cycles to avoid blind spots. Each angle's
sub-agent prompt should demand file:line evidence + fix direction.

### Always-relevant angles
- **Architecture invariants** — do the 8 invariants still hold? Any erosion?
  (read `.dev/AGENTS.md`, verify each in code)
- **Test coverage gaps** — critical paths with zero/paper-thin tests?
- **Error handling consistency** — swallowed errors, blanket 500s, missing context

### Rotating angles (pick based on recent change patterns)
- **Concurrency & resource management** — locks across await, spawned task
  leaks, cancellation safety, channel patterns
- **Configuration & startup robustness** — silent misconfig, missing validation,
  crash-on-first-run
- **Boundary conditions & panics** — overflow, empty input, Unicode, off-by-one,
  timeout edge cases
- **Performance hot paths** — O(n²), unnecessary clones, blocking I/O in async,
  token counting
- **CLI / UX** — exit codes, stdout/stderr hygiene, error message quality
- **Documentation accuracy** — do docs/examples match current code? stale refs?
- **Dependency hygiene** — unmaintained/yanked crates, version conflicts,
  feature bloat, RUSTSEC advisories
- **Public API surface** — over-exposed `pub` items, missing `pub(crate)`,
  pre-0.9 tightening opportunities

### How to pick
- After a feature-heavy sprint → lean toward **test gaps + boundary conditions**
- After a refactor → **invariants + API surface**
- Periodic maintenance → **rotate through all** over several cycles
- After a security advisory → **dependency hygiene + error handling**

---

## Concurrency decision tree

```
Have N independent goals?
│
├─ Do any two touch the same file? → SEQUENTIAL (those two)
│
├─ Do any two change a shared contract (trait/enum/public API)?
│   → SEQUENTIAL (even if files differ)
│
└─ Otherwise → CONCURRENT OK
    (e2e isolation since argusai #9: container names namespaced + random
     host ports — multiple src/ goals can run in parallel without
     container/port collision. Cap at 2-3 concurrent, mind LLM rate
     limits. Monitor each independently with its own watcher.)
```

---

## Quick reference

```bash
# resolve $RR first (recursive repo, not the monorepo root)
if   [ -f .dev/flows/self-improve.flow.js ];        then RR=.          # cwd IS recursive
elif [ -f recursive/.dev/flows/self-improve.flow.js ]; then RR=recursive # cwd is monorepo root
else echo "ERROR: run from the monorepo root or the recursive repo" >&2; exit 1; fi

# Phase 2 — parallel review (agent tool, mode: parallel, one explore per angle)

# Phase 4 — launch a goal (do NOT pre-compile; capture run-id + tmux + log):
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/NNN-*.md --provider deepseek --hitl wecom)

# launch TWO concurrent goals (1 src/ + 1 tests-only, safe pattern):
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/NNN-src-*.md --provider deepseek --hitl wecom)
# wait for run-id capture, then immediately:
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/MMM-tests-*.md --provider deepseek --hitl wecom)
# each gets its own tmux session + worktree. Supervise each independently
# (loop-supervise: watch_file each run's events + schedule_wakeup heartbeat).

# distill a lesson (after the cycle): via Bash + /tmp cp into
# .recursive/skills/loop-supervise/SKILL.md (or this file), then journal it.

# report (don't push unless asked):
git -C "$RR" log --oneline <batch-start-commit>..HEAD
```

---

## Relationship to loop-supervise

| | loop-supervise | self-improve-cycle (this skill) |
|---|---|---|
| Scope | One goal end-to-end | A full review→iterate cycle |
| Review | No (assumes goal exists) | Yes (sub-agents find problems) |
| Concurrency | Single goal | Multiple goals, concurrent-capable |
| Lesson distillation | Manual (user updates) | Built-in (agent auto-updates skills) |
| Trigger | "跑这个 goal" | "找问题跑自迭代" / periodic sweep |

**Load order**: this skill → loads `loop-supervise` → uses its SOP for each
goal. This skill adds the orchestration layer (review, prioritize,
concurrency, distill).

---
type: Skill
name: self-improve-cycle
description: "Drive a full self-improve cycle: architect review (with sub-agents) → generate goals → iterate them through the self-improve flow → verify + report. This is the orchestration layer on top of self-improve-supervise (which handles a single goal). Use when the user wants a periodic/scheduled quality sweep of the recursive codebase, or says '找问题跑自迭代' / 'do a review cycle'. Supports concurrent execution of independent goals across worktrees, and auto-distills lessons learned back into skills."
mode: trigger
triggers: self-improve-cycle, review cycle, 审查周期, 找问题跑自迭代, 定期扫描, quality sweep
---

# self-improve-cycle — Orchestrate a full review→goal→iterate cycle

## What this skill is

The **orchestration layer** on top of
[`self-improve-supervise`](./self-improve-supervise/SKILL.md) (which handles a
single goal end-to-end). This skill drives the **whole cycle**:

```
architect review (sub-agents) → prioritize findings → write goals →
iterate (concurrent if independent) → verify each → distill lessons → report
```

**Load `self-improve-supervise` first** — this skill assumes you know its SOP
(polling, verdict semantics, rescue procedures, the 18 discipline lessons).

## When to use

- The user wants a **quality sweep**: "找问题跑自迭代", "do a review cycle",
  "定期扫描", "看看有什么能改进的"
- **Periodic/scheduled** maintenance (weekly codebase health check)
- After a **major change batch** (verify nothing regressed, find follow-ups)

## When NOT to use

- Single goal execution — use `self-improve-supervise` directly.
- Pure exploration / "explain how X works" — no goal to iterate.
- The user has a **specific** goal already — just supervise it, don't review.

---

## SOP

### Phase 1 — Plan the review (align before diving)

1. **Load `self-improve-supervise`** (its SOP + 18 discipline lessons).
2. **Resolve `$RR`** (step 0a of the supervise skill).
3. **Read `AGENTS.md` + `.dev/AGENTS.md`** for current invariants + contracts.
4. **Check what's changed since last review**: scan recent git log + the
   journal dir (`.dev/journal/`) for hints about what's fresh or fragile.
5. **Pick 3-4 review angles** (see the angle bank below). Tell the user:
   > "I'll review from these angles: A, B, C, D. Expecting ~N goals. Confirm?"
   
   **Wait for confirmation** before diving — don't burn sub-agent budget on
   angles the user doesn't care about.

### Phase 2 — Review (parallel sub-agents)

Launch **3-4 `Explore` sub-agents in parallel** (single message, multiple
tool calls), each scoped to one angle. Each sub-agent prompt:

- State the scope (the angle) + read the invariants first.
- Demand: ranked P0→P3 findings, each with **file:line evidence** + **one-line
  fix direction**. Conclusions only, not file dumps.
- Demand: distinguish "real bug / panic / data loss" from "nice-to-have".

**When sub-agents return**: don't blindly trust — **personally verify the top
2-3 findings** (read the cited code, confirm the bug is real). Sub-agents can
hallucinate file:line or misread intent. Your verification is the gate.

### Phase 3 — Generate goals

Turn verified findings into goal files (`.dev/goals/NNN-*.md`). Rules:

- **One finding = one goal** (don't bundle unrelated fixes — they may fail
  independently and force a preserve).
- **Mirror a recent same-flavour goal's format** (read one neighbour).
- **Each goal MUST have**: Design principle check, Why (root cause + file:line),
  Scope (numbered steps + code), Files NOT to touch, Acceptance (grep-verifiable
  + gate commands), Notes for the agent (traps).
- **Next goal number** = `max(ls .dev/goals/ | grep -oE '^[0-9]+') + 1`.
- **Commit each goal file** before launching (clean-tree mandate).

**Prioritize**: real bugs (panic/crash/data-loss) > invariant/gate holes >
test gaps > docs/cleanup. Tell the user the proposed ordering + scope mix,
confirm, then proceed.

### Phase 4 — Iterate (sequential or concurrent)

#### Sequential mode (default, safest)

Run goals one at a time: launch → background-poll to verdict → verify →
clean up → launch next. This is the simple, safe default. Use it when:
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
2. **e2e gate contention**: the e2e gate uses a **fixed container name
   `recursive-e2e` + port 8080** (`e2e/e2e.yaml:20-22`). Two concurrent e2e
   gates **will collide** on container name + port. So:
   - At most **one concurrent goal** may be a `src/`-touching goal (which
     triggers e2e). The rest must be e2e-skipping (tests/docs only).
   - OR run all goals sequentially if more than one touches `src/`.
3. **LLM rate limits**: each goal's agent + reviewer makes many LLM calls.
   2-3 concurrent goals is usually fine; >3 may hit rate limits.
4. **Monitor each independently**: one background-poll per concurrent goal.
   A crash in one doesn't affect the other (separate tmux sessions).

**Recommended concurrency pattern** (safe default):
- Launch **1 `src/` goal** (triggers e2e, ~15-20min) **+ 1-2 tests/docs goals**
  (e2e skipped, ~5-10min) concurrently. The tests/docs goals finish fast while
  the src/ goal's docker build runs. Max 2-3 concurrent.
- When the batch finishes, launch the next batch.

**Do NOT** run >1 `src/` goal concurrently (e2e container collision).

#### Per-goal workflow (same as self-improve-supervise)

For each goal, concurrent or not:
- Launch via `launch-flow.sh` (capture run-id + tmux + log path).
- Background-poll to terminal (one `run_in_background` per goal).
- On `committed`: verify product independently (run the headline test by name).
- On `failed-preserved`: check if it's a watchdog mis-fire (g353 lesson) →
  cherry-pick rescue. If genuinely broken, report + skip.
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
- **`self-improve-supervise/SKILL.md`** → Discipline section (add a numbered
  lesson) + SOP steps (if it changes the procedure) + Quick reference (if a
  new command pattern emerged).
- **This skill** → if it's about the cycle-level orchestration (review angles,
  concurrency strategy, prioritization).

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
├─ Do any two touch the same file? → SEQUENTIAL (all of them)
│
├─ Do any two change a shared contract (trait/enum/public API)?
│   → SEQUENTIAL (even if files differ)
│
├─ How many touch src/ (trigger e2e gate)?
│   ├─ 0  → all concurrent-safe (but mind LLM rate limits, cap at 3)
│   ├─ 1  → that one solo, OR + 1-2 tests/docs goals alongside
│   └─ 2+ → SEQUENTIAL (e2e container name + port 8080 collision)
│
└─ Otherwise → concurrent OK (cap at 2-3, monitor each independently)
```

---

## Quick reference

```bash
# resolve $RR first (from self-improve-supervise step 0a)
# launch a goal (from self-improve-supervise)
# background-poll to terminal (from self-improve-supervise)

# launch TWO concurrent goals (1 src/ + 1 tests-only, safe pattern):
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/NNN-src-*.md --provider deepseek --hitl wecom)
# wait for run-id capture, then immediately:
(cd "$RR" && .dev/scripts/launch-flow.sh --goal-file .dev/goals/MMM-tests-*.md --provider deepseek --hitl wecom)
# each gets its own tmux session + worktree. Poll each independently.

# distill a lesson (after the cycle):
# edit .zcode/skills/self-improve-supervise/SKILL.md → Discipline section
# add: "**N.** <trigger>. <symptom>. <action>. (incident: gNNN, <commit>)"

# report (don't push unless asked):
git -C "$RR" log --oneline <batch-start-commit>..HEAD
```

---

## Relationship to self-improve-supervise

| | self-improve-supervise | self-improve-cycle (this skill) |
|---|---|---|
| Scope | One goal end-to-end | A full review→iterate cycle |
| Review | No (assumes goal exists) | Yes (sub-agents find problems) |
| Concurrency | Single goal | Multiple goals, concurrent-capable |
| Lesson distillation | Manual (user updates) | Built-in (agent auto-updates skills) |
| Trigger | "跑这个 goal" | "找问题跑自迭代" / periodic sweep |

**Load order**: this skill → loads `self-improve-supervise` → uses its SOP for
each goal. This skill adds the orchestration layer (review, prioritize,
concurrency, distill).

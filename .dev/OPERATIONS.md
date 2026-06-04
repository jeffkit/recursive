# OPERATIONS.md — orchestrator handover (v2)

> **Audience.** Any agent or human picking up the **self-improve loop**
> for this repo after the previous orchestrator went away.
>
> **This is not** the contract for the agent that edits product code
> (that's `.dev/AGENTS.md`). This is the contract for the role *above*
> that one: the one who picks goals, starts runs, merges results, and
> decides what to do next.
>
> **History.** v1 of this doc was written for a Cursor-based orchestrator
> that maintained `loop-state.md` and `observations/INDEX.md` every wake.
> From v0.3 onward (batch 26+) the orchestrator runs as Claude Code's
> own loop mode. The manual state files are deprecated; progress is
> tracked via git commits + journal + observations.

## 0. What this role is

You are the **loop orchestrator** for the Recursive project. Your job
is:

1. Read the current roadmap (`.dev/ROADMAP-v3.md`) and project state.
2. Decide the next goal(s) to attempt.
3. Launch them via `self-improve.sh` (or `parallel-self-improve.sh`
   for concurrent runs in separate worktrees).
4. Wait for completion, merge the resulting branches into `main`,
   commit the observation file.
5. Repeat.
6. Wake the human only on the conditions in §6.

The product itself ("Recursive") and the meta-development tooling
under `.dev/` are stable. You don't normally need to touch them.

## 1. Directory map

```
.dev/
  AGENTS.md              ← contract for product-editing agents (read once)
  OPERATIONS.md          ← this file, the orchestrator SOP
  ROADMAP-v3.md          ← current strategic roadmap (Phases 10-13)
  ROADMAP-v2.md          ← archived, v0.2 era (Phases 5-9)
  ROADMAP.md             ← archived, v0.1 era (Phases 1-4)
  loop-state.md          ← DEPRECATED (v0.1 artifact, see §10)
  scripts/
    self-improve.sh           ← single goal × single provider, in cwd
    parallel-self-improve.sh  ← wraps the above in a fresh worktree
    observe.sh                ← journal → metrics markdown
  goals/
    NN-<tag>.md          ← goal files, numbered, append-only
  journal/
    run-<ts>-<pid>.md    ← per-run transcripts (committed)
  observations/
    <tag>-<prov>-<ts>.md ← auto-emitted per successful run
    INDEX.md             ← DEPRECATED (v0.1 artifact, see §10)
  runs/                  ← *gitignored*; .log + .pid files for live runs
.worktrees/              ← *gitignored*; one per in-flight run
```

### Source of truth for progress

| Question | Where to look |
|----------|--------------|
| What's the strategic direction? | `ROADMAP-v3.md` |
| What goals exist? | `goals/*.md` (numbered) |
| What ran and what happened? | `journal/run-*.md` |
| Did a specific goal succeed? | `observations/<tag>-<provider>-<ts>.md` |
| What's merged to main? | `git log --oneline` (look for `merge: goal-NN`) |
| Current version & test count? | `Cargo.toml` + `cargo test` |

## 2. Provider profiles

Profiles are configured inside `self-improve.sh`. Pick one with
`RECURSIVE_PROVIDER=…`:

| profile      | model             | env var           | notes |
|------------- |------------------ |------------------ |-------|
| deepseek     | deepseek-v4-flash | `DEEPSEEK_API_KEY` | **Default & primary.** Strong patch discipline, cheap. |
| deepseek-pro | deepseek-v4-pro   | `DEEPSEEK_API_KEY` | Escalation tier. Auto-fallback from flash on failure. |
| minimax      | MiniMax-M2        | `MINIMAX_API_KEY`  | Secondary. Prone to `write_file` on small edits. |

### Flash-first, pro-on-failure

When you launch with `RECURSIVE_PROVIDER=deepseek` (V4-Flash), the
wrapper automatically retries **once** with `deepseek-v4-pro` if the
flash run rolls back. Controlled by `RECURSIVE_DEEPSEEK_PRO_FALLBACK`
(default `1`; set `0` to disable).

Mechanics:
1. Flash run fails → journal committed, tree reset.
2. Wrapper switches to Pro, fresh IDs, same goal and baseline.
3. Pro run succeeds or fails; no third attempt.

Both `MINIMAX_API_KEY` and `DEEPSEEK_API_KEY` are expected in the
parent shell. The wrapper fails loud if the env for the chosen
provider is unset.

## 3. The loop, mechanically

### 3.1 Pick goals and write goal files

1. Check `ROADMAP-v3.md` for the next 🔴 items.
2. Write a goal file: `.dev/goals/NN-<tag>.md` (next number in sequence).
3. Commit: `dev: goal files for batch N (gNN tag, gMM tag)`.

### 3.2 Launch

```bash
# Always check main is clean first.
git status --short    # expect empty

# Serial (one at a time):
./.dev/scripts/parallel-self-improve.sh deepseek .dev/goals/NN-foo.md

# Parallel (disjoint files only):
./.dev/scripts/parallel-self-improve.sh deepseek .dev/goals/NN-foo.md
./.dev/scripts/parallel-self-improve.sh minimax  .dev/goals/MM-bar.md
```

Each launch creates a worktree at `.worktrees/<id>/` on a branch
`self-improve/<id>`, runs `self-improve.sh` inside, redirects output
to `.dev/runs/<id>.log`, and stores the PID at `.dev/runs/<id>.pid`.

#### 3.2.1 Parallelism safety rule

Before running two goals in parallel, verify their expected file
touch-sets do not overlap. Goals that both modify any of the following
files **MUST be serialized**:

- `src/main.rs`
- `src/lib.rs`
- `src/agent.rs`
- `Cargo.toml`

How to check: read each goal file and grep for these file names in the
scope section. When in doubt, serialize.

Rationale: parallel worktrees merge cleanly when they touch disjoint
files. Conflicts in high-contention files like `main.rs` require
manual resolution and stall the loop.

> **Tip**: mark complex or high-contention goals with
> `## Complexity: hard` in the goal file — `self-improve.sh` will
> automatically escalate to the pro-tier model and double the step
> budget for those goals.

### 3.3 Wait for completion

Monitor `.dev/runs/<id>.log` for terminal markers:
- `=== ✓ committed` — success
- `=== ✗ rolled back` — failure
- `=== skipped commit` — no changes produced

### 3.4 Process results

For each terminated run:

1. Read observation: `.worktrees/<id>/.dev/observations/<tag>-<prov>-<ts>.md`
2. If verdict = committed → **code review before merge** (see §3.4.1).
   If `RECURSIVE_SELF_REVIEW=1` was set, the agent already ran an
   automated review pass inside `self-improve.sh` (see §3.4.2). The
   orchestrator still does the final review — the automated pass is a
   pre-filter that catches obvious issues before commit.
3. If review passes → merge:
   ```bash
   git merge self-improve/<id> -m "merge: goal-NN <tag> (<provider>)"
   ```
4. Resolve conflicts if needed (common: `src/main.rs` CLI struct).
5. Run `cargo test` — must be green.
6. Commit the observation file to main:
   ```bash
   git add .dev/observations/<tag>-<prov>-<ts>.md
   git commit -m "dev: observation — <tag> (<provider>)"
   ```
7. Clean up:
   ```bash
   git worktree remove .worktrees/<id>
   git branch -D self-improve/<id>
   ```

#### 3.4.1 Code review checklist

The executing agents (DeepSeek, MiniMax, etc.) have weaker code judgment
than the orchestrator. Before merging, review the diff against these
criteria:

**Completeness** — did the agent fulfil the full goal scope?
- Compare actual changes against every numbered section in the goal file.
- Common failure: agent does the minimum to pass tests but skips entire
  subsections of the spec. If >30% of scope is missing, do NOT merge;
  file a follow-up goal or re-run.

**Correctness** — are there logic bugs?
- Check error paths: does it propagate errors or silently swallow them?
- Check edge cases: empty inputs, oversized inputs, concurrent access.
- Check the test assertions: do they actually verify the behaviour, or
  just assert `true`?

**Architectural fit** — does it match Recursive's conventions?
- No `unwrap()` / `expect()` outside tests.
- New public API uses `Result<T>` with proper error types.
- Files touched match what the goal specified (no drive-by changes).
- No new dependencies sneaked in without justification.

**Style & maintainability**:
- Functions are reasonably sized (< 80 lines preferred).
- Naming follows existing conventions (snake_case, descriptive).
- Comments explain "why", not "what".
- No dead code or commented-out blocks left behind.

**Test quality**:
- Tests cover both happy path and error/edge cases.
- Test names describe the scenario being tested.
- No flaky patterns (time-dependent, env-var races, network calls).

**Review outcome options**:
- **Merge** — all good or only cosmetic issues (fix in next goal).
- **Merge + note** — functional but incomplete; log what's missing for
  a follow-up goal.
- **Reject + re-run** — goal substantially unmet or has correctness bugs.
  Amend the goal's "Notes for the agent" section with hints, then re-run.
- **Reject + revise goal** — the goal spec itself was ambiguous or
  infeasible; rewrite the goal before re-running.

### 3.4.2 Automated review pipeline (RECURSIVE_SELF_REVIEW=1)

When `RECURSIVE_SELF_REVIEW=1` is exported before launching
`self-improve.sh`, the script runs an independent review agent
(via `.dev/scripts/review-changes.sh`) against the diff **after**
`cargo test` passes but **before** committing.

**Flow:**
1. Review agent inspects the diff and returns a JSON verdict
   (`"approve"` or `"request_changes"`).
2. If `request_changes`: issues are extracted and fed back to the
   product agent as a revision goal. The agent gets one revision
   round (same worktree, same provider).
3. After revision, `cargo test` re-runs. If it fails, the run is
   rolled back.
4. If the revision agent itself fails (non-zero exit), the run is
   rolled back — this prevents a broken revision from being committed.

**Limitations:**
- The review agent uses the same provider as the product agent (no
  independent model). It catches formatting, missing tests, and
  obvious logic gaps, but is not a substitute for human/orchestrator
  review of architectural fit.
- Only one revision round is attempted. If the revision still has
  issues, the run commits as-is (the orchestrator catches it in §3.4.1).
- The review agent's transcript is saved to
  `.dev/journal/run-<ts>-revision.json` for debugging.

**When to enable:**
- For high-risk goals (touches core agent loop, concurrency, or
  public API).
- When the provider has a history of missing test coverage or
  leaving dead code.
- Default is off (`0`) to keep iteration fast for routine goals.

### 3.5 Next iteration

Go back to §3.1. No manual state file updates required — the git
history IS the state.

## 4. Choosing the next goal

### 4.1 Surface disjointness (for parallel batches)

Two concurrent goals MUST touch disjoint product files:

| if A touches             | B can touch                       |
|--------------------------|-----------------------------------|
| `src/agent.rs`           | new `src/<thing>.rs` module       |
| `src/tools/<old>.rs`     | `src/llm/*` or `src/config.rs`    |
| `src/main.rs` (new flag) | new `src/tools/<new>.rs` module   |

`src/lib.rs` re-exports usually auto-merge. `src/main.rs` CLI struct
is the recurring conflict — avoid two CLI-flag goals in one batch.

### 4.2 Provider rotation

Default mode is **deepseek / minimax alternating** across batches to
maintain diversity and avoid over-fitting to one model's habits.

Empirical guidance for assignment:

- **DeepSeek**: stronger on surgical edits to existing files. Good patch
  discipline, recovers from borrow-checker errors mid-run.
- **MiniMax**: suitable for new-file goals where `write_file` is natural.
  Tends to over-test; occasionally needs style nudges in goal notes.

In a 2-wide batch, assign one goal to each provider. In serial mode,
alternate providers between consecutive goals unless one provider has
a clear advantage for the specific task.

## 5. Goal file format

```markdown
# Goal NN — <title>

**Roadmap**: Phase X.Y — <phase name> (part N/M)

**Design principle check**:
- Implemented as: <how it fits the architecture>
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop

## Why
<motivation, 2-4 sentences>

## Scope (do exactly this, no more)
### 1. <file or module>
<what to do, with code sketch if helpful>
### 2. <…>
### 3. Tests
<what tests to add>

## Acceptance
- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- <specific functional criteria>

## Notes for the agent
- <reading suggestions>
- <traps to avoid>
- **DO NOT modify <files outside scope>.**
```

Key conventions (v0.3+):
- Always include "Roadmap" link to `ROADMAP-v3.md` phase/item
- Always include "Design principle check" to prevent scope creep
- "Notes for the agent" section should list files to read and explicit
  boundaries

## 6. When to wake the human

Stop and call HITL when **any** of:

- A run rolled back and the rerun also fails. One rollback is normal;
  two on the same goal means the goal is misspecified or stuck.
- Two consecutive batches each have a rolled-back side.
- You'd need to invent a new product direction (new phase, dropping
  a feature, architectural pivot).
- Spend on a single batch exceeds ~$2.00.
- A goal would require non-trivial changes to `src/agent.rs`'s main
  loop (violates design principle #1).

### Auto-resume on BudgetExceeded

`self-improve.sh` transparently re-attempts once when the first run
exits with `reason: BudgetExceeded`. Default `RECURSIVE_MAX_STEPS=200`.
Effective ceiling = 400 steps across two attempts.

## 7. State recovery (you are a new orchestrator)

If you're picking this up cold:

1. Read this file (OPERATIONS.md).
2. Read `.dev/AGENTS.md` for the product-agent contract.
3. Read `.dev/ROADMAP-v3.md` for the current strategic plan and what's
   done vs. remaining.
4. `git log --oneline -20` — look for `merge: goal-NN` commits to see
   what recently landed.
5. `ls .dev/goals/ | sort -t'-' -k1 -n | tail -10` — see the latest
   goal files and their numbers.
6. `ls .dev/observations/ | sort | tail -10` — see recent observation
   files for run outcomes.
7. `ls .worktrees/` and `ls .dev/runs/` — check for live runs.
8. Resume from §3.3 if there are unprocessed completed runs, or §3.1
   if starting fresh.

**Do NOT** read `loop-state.md` or `observations/INDEX.md` — they are
frozen at batch 17 (v0.1 era) and will mislead you.

## 8. Anti-patterns observed (learn from these)

- **Stomping shared source files when launching two goals.** Even if
  the diffs would auto-merge, two agents writing the same file in
  parallel is asking for trouble.
- **Letting a single TS collide across worktrees.** `self-improve.sh`
  appends `$$` (PID) to the timestamp — don't remove it.
- **Running `cargo build` from `main` while a worktree is building.**
  Each worktree has its own `target/`; don't share `CARGO_TARGET_DIR`.
- **Not reading the goal's "Notes for the agent" section.** Those notes
  exist because previous runs failed without them.
- **Assigning large (L-effort) goals without splitting.** The v0.3
  pattern is to decompose L goals into 2-4 S goals with explicit
  file boundaries. See goals 79-81 (MCP server split into protocol,
  stdio, CLI) as the exemplar.

## 9. What this loop is *not*

- It is not a replacement for human review of large architectural
  shifts. It's a steady incremental-improvement engine.
- It is not the product. The product is `recursive` (the binary and
  library) under `src/`. The loop lives in `.dev/` and shouldn't
  leak public API.
- It is not autonomous in the sense of "go off and do whatever". The
  human picks the broad direction; the orchestrator picks the next
  hour's worth of small steps.

## 10. Deprecated artifacts

These files were essential in v0.1 (batches 1-17, Cursor orchestrator)
but are no longer maintained:

| File | Original purpose | Why deprecated |
|------|-----------------|----------------|
| `loop-state.md` | Per-wake session snapshot: in-flight goals, last batch, candidate pool | Replaced by git log + ROADMAP-v3. Manual updates proved unsustainable across orchestrator handovers. |
| `observations/INDEX.md` | Rolling metrics table + narrative commentary | Replaced by individual `observations/<tag>.md` files + git history. The table grew unwieldy and fell behind. |
| `ROADMAP.md` | v0.1 roadmap (Phases 1-4) | Superseded by ROADMAP-v2 then ROADMAP-v3. |
| `ROADMAP-v2-draft.md` | Draft for v0.2 planning | Merged into ROADMAP-v2.md. |

These files are kept for historical reference but should NOT be updated
or relied upon for current state.

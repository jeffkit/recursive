# OPERATIONS.md — orchestrator handover

> **Audience.** Any agent or human picking up the **self-improve loop**
> for this repo after the previous orchestrator went away.
>
> **This is not** the contract for the agent that edits product code
> (that's `.dev/AGENTS.md`). This is the contract for the role *above*
> that one: the one who picks goals, starts runs, merges results, and
> decides what to do next.

## 0. What this role is

You are the **loop orchestrator** for the Recursive project. Your job
is:

1. Look at the project's state and the rolling observation file.
2. Decide the next pair of self-improve goals to attempt.
3. Launch them in isolated git worktrees, one per provider.
4. Wait for completions, merge the resulting branches into `main`,
   update the observation index.
5. Repeat.
6. Wake the human only on the conditions in §6.

The product itself ("Recursive") and the meta-development tooling
under `.dev/` are stable. You don't normally need to touch them.

## 1. Directory map

```
.dev/
  AGENTS.md            ← contract for product-editing agents (read once)
  OPERATIONS.md        ← this file, the orchestrator SOP
  loop-state.md        ← current session snapshot (refresh each wake)
  scripts/
    self-improve.sh         ← single goal × single provider, in cwd
    parallel-self-improve.sh ← wraps the above in a fresh worktree
    observe.sh              ← journal → metrics markdown
  goals/
    NN-<tag>.md        ← goal files, numbered, append-only
  journal/
    run-<ts>-<pid>.md  ← per-run transcripts (committed)
  observations/
    INDEX.md           ← rolling per-run metrics + commentary
    <tag>-<prov>-<ts>.md ← auto-emitted per successful run
  runs/                ← *gitignored*; .log + .pid files for live runs
.worktrees/            ← *gitignored*; one per in-flight run
```

## 2. Provider profiles

Profiles are configured inside `self-improve.sh`. Pick one with
`RECURSIVE_PROVIDER=…`:

| profile        | model              | env var          | notes                       |
| -------------- | ------------------ | ---------------- | --------------------------- |
| minimax        | MiniMax-M2         | `MINIMAX_API_KEY` | default. Prone to `write_file` on small files. |
| deepseek       | deepseek-v4-flash  | `DEEPSEEK_API_KEY` | **Default DeepSeek tier.** Strong patch discipline; cheaper per token. |
| deepseek-pro   | deepseek-v4-pro    | `DEEPSEEK_API_KEY` | Escalation tier — use directly or let flash-fallback pick it up. |
| glm            | glm-5.1            | `GLM_API_KEY`    | First serious GLM run on 5.1 from batch 9. glm-4-flash earlier was unable to drive a tool loop (one read_file then "stop" as final answer). Pricing placeholder until calibrated against Zhipu billing. |

Legacy `deepseek-chat` is accepted as an alias for `deepseek-v4-flash`
until the DeepSeek API retires it (2026-07-24).

### 2.1 DeepSeek flash-first, pro-on-failure

When you launch with `RECURSIVE_PROVIDER=deepseek` (V4-Flash), the wrapper
automatically retries **once** with `deepseek-v4-pro` if the flash run
rolls back (agent failure or post-run `cargo test` red). Controlled by
`RECURSIVE_DEEPSEEK_PRO_FALLBACK` (default `1`; set `0` to disable).

Mechanics:

1. Flash run fails → journal committed, tree reset to baseline (normal
   rollback).
2. Wrapper switches model to Pro, fresh journal/transcript IDs, same goal
   and baseline — **no transcript carry-over** from the failed flash run.
3. Pro run succeeds or fails on its own; there is no third attempt.

To force Pro from the start: `RECURSIVE_PROVIDER=deepseek-pro` or
`parallel-self-improve.sh deepseek-pro …`. Explicit Pro skips the
flash-first path.

Orchestrator note: when a flash run rolls back and the log shows the
`retrying with deepseek-v4-pro` line, **wait for the pro pass** before
HITL — the worktree may still be running.

Both `MINIMAX_API_KEY` and `DEEPSEEK_API_KEY` are expected to be
present in the parent shell. The wrapper script will fail loud if
the env for the chosen provider is unset.

## 3. The loop, mechanically

### 3.1 Start a batch (concurrent pair)

```bash
# Always check main is clean first.
git status --short    # expect empty

./.dev/scripts/parallel-self-improve.sh deepseek .dev/goals/NN-foo.md
./.dev/scripts/parallel-self-improve.sh minimax  .dev/goals/MM-bar.md
```

Each launch creates a worktree at `.worktrees/<id>/` on a branch
`self-improve/<id>`, runs `self-improve.sh` inside, redirects all
output to `.dev/runs/<id>.log`, and stores the PID at
`.dev/runs/<id>.pid`.

**Surface rule.** The two goals you launch concurrently MUST touch
disjoint product files. Otherwise the merge will conflict and you'll
spend more time resolving than parallelism saved. Reread each goal's
"Scope" section before launching.

### 3.2 Arm the wake signal

Run one persistent polling watcher in the background that scans
`.dev/runs/*.log` for `=== ✓ committed`, `=== ✗ rolled back`, or
`=== skipped commit`, emits a sentinel line like:

```
AGENT_LOOP_WAKE_self_improve {"run":"<id>","verdict":"=== ✓ committed"}
```

…and touches `<log>.notified` so it doesn't refire.

In Cursor, attach a `notify_on_output` matcher on
`^AGENT_LOOP_WAKE_self_improve` to that background shell. Also arm a
30-minute fallback heartbeat (`sleep 1800 && echo … sentinel`) so a
silently-broken watcher doesn't freeze the loop.

The `.notified` markers live under `.dev/runs/` which is gitignored —
they don't pollute history. Re-mark old logs before starting the
watcher so it doesn't replay completed events.

### 3.3 Wake → process

On every wake:

1. Find which logs are newly terminated (have a `=== …` line and
   were modified since the previous wake; or just check
   `.dev/runs/*.log.notified` for new markers).
2. For each terminated run:
   - Read its observation file inside the worktree:
     `.worktrees/<id>/.dev/observations/<id>.md`.
   - Read tail of its `.dev/runs/<id>.log` for the cost + finish-reason
     line.
3. Decide whether to merge (almost always yes if `verdict = committed`;
   roll back the worktree and HITL otherwise).
4. From `main`, merge each branch with `--no-ff`:
   ```bash
   git merge --no-ff self-improve/<id> -m "merge: goal-NN <tag> (<provider>)"
   ```
   Resolve conflicts. The common conflict is `src/main.rs` (CLI
   struct + `run_once` signature collecting flags from both goals);
   it's a manual 4-block stitch.
5. `cargo test` — must be green.
6. Remove worktrees + branches:
   ```bash
   git worktree remove .worktrees/<id>
   git branch -D self-improve/<id>
   ```
7. Update `.dev/observations/INDEX.md`: add a row to the summary
   table for each merged run, add a short narrative under the
   per-run notes section.
8. Update `.dev/loop-state.md` to reflect the new "currently in
   flight" set (empty if the whole batch landed).
9. Pick the next pair (§4) and go back to §3.1.

If only one run of the pair has terminated, **wait for the other
one** before merging — landing one mid-batch makes the other branch's
baseline stale and forces a rebase. Just acknowledge the partial
event and keep waiting.

## 4. Choosing the next goal pair

Two heuristics, applied in order.

### 4.1 Surface disjointness

The two goals MUST plausibly touch different files. Common easy
disjoint pairs:

| if A touches             | B can touch                       |
| ------------------------ | --------------------------------- |
| `src/agent.rs`           | new `src/<thing>.rs` module       |
| `src/tools/<old>.rs`     | `src/llm/*` or `src/config.rs`    |
| `src/main.rs` (new flag) | new `src/tools/<new>.rs` module   |
| `src/config.rs` (small)  | anything except `config.rs`       |

`src/lib.rs` re-exports get touched by almost every goal — they
usually auto-merge cleanly because the additions are on different
lines. Watch for `src/main.rs`'s CLI struct: most CLI-flag goals
collide there, so try not to ship two CLI-flag goals in the same
batch.

### 4.2 Provider strengths

Empirical (from the runs already in `INDEX.md`):

- **DeepSeek** (`deepseek-chat`): patient with tests, holds
  apply_patch discipline. Best for multi-file changes where the
  scope is well-defined. Costs more in absolute dollars per run
  because of prompt accumulation.
- **MiniMax** (`MiniMax-M2`): faster, cheaper per run, but reaches
  for `write_file` on small files even when the goal says otherwise.
  Best for green-field new modules or tiny one-file edits where the
  apply:write ratio matters less.

Try not to give the same provider two consecutive batches of the
same flavour — you want comparable data across providers eventually.

### 4.3 Topic prioritisation

When in doubt, prefer goals that:

1. Follow up on an observation that just landed (e.g., add caching
   right after we noticed prompt-token amplification).
2. Stay under ~150 LOC product change so a single-step rollback is
   cheap if something goes wrong.
3. Have clear "Definition of done" with cargo-testable behaviour.

Avoid in this role:

- Goals that rewrite `src/agent.rs`'s core loop. Schedule those with
  the human first — it's load-bearing.
- Goals that add external service dependencies (auth tokens, network
  endpoints).
- "Refactor everything" goals. Slice them.

The current goal pool / completed list is in `INDEX.md` and
`loop-state.md`'s "next candidates" section.

## 5. Goal-file conventions

Every goal file under `.dev/goals/` MUST include this header (above
any other section) **starting from batch 12 onward**:

```
# Goal NN — <title>

**Roadmap**: <id> — <feature>   |   dev-infra   |   chore

**Design principle check**:
- Implemented as: [new Tool | new LlmProvider | new StepEvent observer
                   | system prompt source | new module that the agent
                   loop *calls into*]
- ❌ Does NOT branch inside `agent.rs::Agent::run`'s main loop
```

This is the contract that keeps the kernel orthogonal as features pile
on. Goals lacking the header should be REJECTED before launching
(amend the goal file, then proceed). Pre-batch-12 goal files
(04-30) are grandfathered — only new ones are subject to this rule.

Rest of the goal file:

```
## Why
## Scope (do exactly this, no more)
### 1. <file or area>
### 2. <…>
### 3. Tests
## Acceptance (was: Definition of done)
## Notes for the agent
```

The product-editing agent is bound by `.dev/AGENTS.md`. Anything in
"Notes for the agent" supplements but does not override that
contract.

Be explicit. Vague goals waste tokens (and money).

### 5.1 Roadmap ↔ loop-state ↔ goals — coordination protocol

Four documents, distinct rhythms:

| Doc | Role | Update rhythm | Updater |
|---|---|---|---|
| `.dev/ROADMAP.md` | Strategic plan, design principle, ❌ "won't build" list, status column (✅/🟡/🔴/⏸️) | When a batch lands → flip status; rare strategic edits | Orchestrator on land; user for direction shifts |
| `.dev/loop-state.md` | Session snapshot: in-flight, last batch landed, roadmap delta, candidate pool | Every wake | Orchestrator |
| `.dev/goals/NN-*.md` | Single task spec with roadmap link in header | When picking next batch | Orchestrator |
| `.dev/observations/INDEX.md` + `<id>.md` | Per-run metrics archive | After each merge | observe.sh + orchestrator catch-up |

Trigger relationships:

```
user edits ROADMAP.md (rare) ─┐
                              ├→ orchestrator must pick next batch
goal merges to main ──────────┤  from 🔴-status items
                              │
                              ├→ flip ROADMAP.md status to ✅ <commit>
                              ├→ update loop-state.md "last batch"
                              ├→ update loop-state.md "Roadmap delta"
                              └→ append INDEX.md row + per-run file

phase complete (all Phase-N items ✅) ─→ HITL: send phase summary
                                          to user, await direction
                                          on next phase
```

What the orchestrator does NOT touch in ROADMAP.md without HITL:
- Adding new feature rows (phase scope is a user-level decision)
- Removing rows from "What NOT to Build"
- Reordering phases
- Changing Effort or Impact estimates substantially

What the orchestrator MAY freely edit in ROADMAP.md:
- Status column (flipping 🔴/🟡/✅/⏸️)
- Status-row commentary parenthetical (e.g. "promoted from medium")
- "Phase 0" historical record under Priority Matrix

## 6. When to wake the human

Stop and call HITL (via the appropriate channel for the environment;
in this project that's the `hitl` skill / `mcp2cli @hitl`) when **any**
of:

- A run rolled back (`=== ✗ rolled back`) and the rerun also fails.
  One rollback is normal; two on the same goal means the goal is
  misspecified or the agent is stuck.
- Two consecutive batches each have a rolled-back side.
- You'd otherwise need to invent a new product direction (streaming,
  switching language, dropping a feature, open-sourcing decisions).
- Spend on a single batch exceeds ~$1.50 (sanity check; not a hard
  budget). Cost is in the per-run observation footer. Note: with
  auto-resume enabled (see below), a single goal can pay up to 2×
  the per-attempt cost when it BudgetExceeds and replays.
- A goal would require touching `src/agent.rs`'s main loop in a
  non-trivial way.

### Auto-resume on BudgetExceeded

`self-improve.sh` will, by default, transparently re-attempt a goal
once when the first run exits with `reason: BudgetExceeded`. The
resumed run is seeded with the full saved transcript via
`recursive replay --resume-from N <goal>` (goal-17 plumbing).

- Default `RECURSIVE_MAX_STEPS=200` (matches Cursor's per-turn ceiling;
  was 100 before batch-13, was 50 before goal-30 batch).
- Effective ceiling on a hard goal = 400 steps across two attempts.
- Resume is **once only**. If both attempts BudgetExceed, the run
  rolls back exactly as before.
- `observe.sh` reports `auto-resumed: yes/no` in the metrics table
  and uses the *last* termination reason as truth (so a recovered
  run shows `NoMoreToolCalls`, not the transient BudgetExceeded).
- Disable per-run with `RECURSIVE_AUTO_RESUME=0` if you want to
  characterize raw single-shot performance of a provider/goal pair.
- 10 successful merges have accumulated since the last human-facing
  summary. Send a short status note.

For pure progress reporting on the *terminal of the orchestrator*
(Cursor chat), one-line updates are fine — don't HITL just to say
"goal NN landed".

## 7. State recovery (you are a new orchestrator)

If you're picking this up cold:

1. Read this file.
2. Read `.dev/AGENTS.md` for the product-agent contract.
3. Read `.dev/observations/INDEX.md` for the rolling run log.
4. Read `.dev/loop-state.md` for what was in flight at handover.
5. `git status` + `git log --oneline -20` to see uncommitted work
   and recent commits.
6. `ls .worktrees/` and `ls .dev/runs/` to see live runs, if any.
7. Resume from §3.3 ("Wake → process") if there are unprocessed
   completed runs, or §3.1 if you're starting a fresh batch.

## 8. Anti-patterns observed (learn from these)

- **Touching `.dev/.last-provider` during parallel runs.** The
  rotation file is meant for serial use; concurrent worktrees use
  `RECURSIVE_PROVIDER=…` explicitly. Don't try to "make rotation
  work concurrently".
- **Stomping shared source files when launching two goals.** Even if
  the diffs would auto-merge, two agents writing the same file in
  parallel is asking for trouble. Surface-disjoint pairing is non-
  negotiable.
- **Letting a single TS collide across worktrees.** `self-improve.sh`
  appends `$$` (PID) to the timestamp for exactly this reason — don't
  remove it.
- **Running `cargo build` from `main` while a worktree's
  `self-improve.sh` is also building.** Each worktree has its own
  `target/`; that's fine. But don't share `CARGO_TARGET_DIR` across
  worktrees.
- **Forgetting to update `INDEX.md` after a merge.** Future-you (or
  the next orchestrator) reads it to make decisions. Skipping is a
  silent integrity bug.

## 9. What this loop is *not*

- It is not a replacement for human review of large architectural
  shifts. It's a steady incremental-improvement engine.
- It is not the product. The product is `recursive` (the binary and
  library) under `src/`. The loop lives in `.dev/` and shouldn't
  leak public API.
- It is not autonomous in the sense of "go off and do whatever". The
  human picks the broad direction; the orchestrator picks the next
  hour's worth of small steps.

That's the whole job.

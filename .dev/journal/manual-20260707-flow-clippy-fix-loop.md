# Manual edit: flow-clippy-fix-loop

**Date**: 2026-07-07
**Goal**: Make clippy (and other gate) lints visible and fixable by the agent in the self-improve flow, so weak executor models don't burn all fix rounds blind and land `failed-preserved` on mechanical lints.
**Files touched**: `.dev/flows/self-improve.flow.js`
**Tests added**: none (flow meta-tooling; verified via `node --check` + load smoke)

## What changed

1. **`HEADLESS_CONSTRAINT` now mandates self-gates before stopping.** The agent
   must run `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`,
   and `cargo test --workspace` itself before declaring done. The flow's gates
   become a backstop, not the first check. Includes a small cookbook of common
   clippy fixes (`unwrap_used` on `Mutex::lock()` → `unwrap_or_else(|e| e.into_inner())`,
   `empty_line_after_doc_comments`, `cloned_ref_to_slice_refs`) so weak models
   have an explicit path instead of guessing.

2. **`runQualityGates` fix-loop stderr feeding rewritten** (`buildFixGoal`):
   - Old: `lastOutput.slice(-3000)` — only the tail. clippy/cargo emit `error:`
     lines at the **head**, so earlier lints (doc-comment, from_ref) were
     truncated away and the agent literally could not see them.
   - New: write the **full** gate output to `.gate-<name>-output.log` in the
     worktree (agent can `Read` it) **and** inline-extract every
     `error:`/`warning:`/`--> file:line:col` line (capped ~6KB) so the agent
     sees the complete actionable set on the first turn.

3. **`GATE_FIX_HINTS`**: per-gate fix guidance. `clippy` gets the
   unwrap/empty-line/from_ref cookbook; `test` gets doctest-struct-field
   guidance. Lowers the chance a weak fixer model spins on the wrong fix.

4. **`worktreeDirty` no-edit guard**: after each fix round, if the worktree has
   zero changes the agent didn't actually edit anything (common with weak
   models that just re-run the gate) — break early with
   `reason: "agent made no edits in fix round N"` instead of burning all
   `MAX_FIX_ROUNDS` on identical stderr. Makes the `failed-preserved` reason
   diagnostic.

5. **`runAttempt`/`runAttemptWithGoal` gate catches** now prefer `err.reason`
   (the fine-grained cause from runQualityGates) over the generic
   "N fix rounds" string, so preserve scenes say *why*.

## Why

G324's deepseek-v4-flash run landed `failed-preserved` after clippy held 3
fix rounds. Root cause: the agent's 5 `Mutex::lock().unwrap()` lints sat at
the head of the clippy output and were truncated by `slice(-3000)`, so the
fixer only ever saw the last couple of errors and couldn't fix the ones it
never saw. Combined with flash not running clippy itself, every round was
blind. The 6 offending lints were mechanical (5 unwrap → poison-recovery,
1 doc-comment blank line, 1 from_ref) and were fixed by hand in a follow-up
commit (fc1e5a6) to land G324. This change makes the flow itself feed the
agent the full error set + a cookbook, and makes the agent self-verify so
the gates aren't the first time anyone runs clippy.

## Notes

- G324 itself landed as `b9a925e` (agent's 674-line feature) + `fc1e5a6`
  (hand clippy cleanup). Preserved scene was consumed and cleaned up
  (worktree removed, `refs/preserve/...` deleted, branch merged + deleted).
- A stale partial G324 attempt was found uncommitted in the main checkout
  (leftover from an earlier rolled-back run) — stashed as `stash@{0}`
  ("stale: pre-g324 leftover...") as a safety net, not dropped.

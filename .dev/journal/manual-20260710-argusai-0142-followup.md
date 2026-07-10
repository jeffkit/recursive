# Manual edit: argusai 0.14.2 follow-up (issue #8 fixed)

**Date**: 2026-07-10
**Goal**: Adopt the upstream fix for argusai issue #8, verify the local
workarounds, and clean up any latent defects the hardened gate now exposes.

## What 0.14.2 fixed upstream

argusai-mcp 0.14.2 (released 2026-07-10) closes issue #8:

- Case events now carry a stable `suiteId` sourced from `e2e.yaml`'s `id`.
- `argus_run` aggregates by `suiteId`, not by the free-text `name` string.
- Declaring cases but aggregating none is now recorded as a **failure**
  (no more silent false-green).

## Verification

- Upgraded global `argusai-mcp` to 0.14.2 (`argusai-core` 0.14.2).
- Proved #8 fixed: ran `smoke` with its `e2e.yaml` name deliberately
  mismatched against the suite yaml name → `totals={passed:3,failed:0,total:3}`.
  Under 0.14.1 this exact mismatch dropped every case (total=0, false green).
- 0.14.2 makes the e2e.yaml↔file name alignment (commit 7f2091a) **no longer
  load-bearing**. The aligned names are kept as a convention; the
  `totals.total > 0` guard in `e2e-gate.sh` / `e2e-run.sh` stays as
  defense-in-depth alongside the upstream "empty aggregation = failure".

## Latent defect the hardened gate exposed

Fixing the false-green surfaced a real, pre-existing test defect in
`e2e/tests/40-claude-json-stream.yaml` that 0.14.1's total=0 had been hiding:

- Case `multiturn: result num_turns rose to 3` asserted `grep -q
  '"num_turns":3'`. But `claude_json.rs` (`JsonEventTask::finish`,
  `num_turns = if steps > 0 { steps } else { self.num_turns }`) reports
  **that turn's step count** in each `result` event, and stream-json emits
  one `result` per turn. A 2-turn run yields `num_turns` 2 then 1 — never 3.
- Single-turn stream: 1 `result`, `num_turns=2`.
  Multi-turn stream: 2 `result` events, `num_turns=2` and `1`.
- The follow-up-turn intent is already proven by the "distinct answer"
  case (`grep 'I just created greet.txt'`). Replaced the bogus `num_turns:3`
  assertion with a stronger one: count `result` events and require `>= 2`
  (single-turn produces exactly 1, so ≥2 is a reliable multi-turn signal).
- Updated the suite description to document the stream-json `num_turns`
  semantics so the next reader doesn't re-introduce the same wrong assumption.

**Files touched**:
- `e2e/tests/40-claude-json-stream.yaml` — fixed multiturn case + description.
- `.dev/scripts/e2e-gate.sh` — comment: 0.14.2 fix, name alignment now
  convention-only, total>0 guard retained as defense-in-depth.
- `.dev/scripts/e2e-run.sh` — same comment refresh.

**Tests added**: none (existing suite corrected; 12/12 green under 0.14.2).

**Notes**:
- Product gap worth a separate goal: stream-json emits one `result` per
  turn and `num_turns` reports per-turn step count, whereas the Claude
  Agent SDK emits a single terminal `result` with a run-wide turn count.
  Aligning that is a product change (locked by `claude_json.rs` unit test
  `num_turns==2` for a 2-step turn) and is out of scope for this follow-up.
- No Rust source changed; cargo gates unaffected.

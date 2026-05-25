# Observations

Cross-run metrics for self-improve cycles. Auto-extracted from
`.dev/journal/run-*.md` by `.dev/scripts/observe.sh`. Per-run detail
files in this folder; this index is the side-by-side comparison.

## Successful runs

| goal | provider | model | steps | tool calls | err results | apply:write | reason |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 04 token-usage | minimax | MiniMax-M2 | 43 | 42 | 10 | 10:7 | NoMoreToolCalls |
| 05 apply-patch-unified | deepseek | deepseek-chat | 23 | 23 | 6 | 6:3 | NoMoreToolCalls |

## Observations so far

### MiniMax-M2 — goal 04 (token usage)

- **43 steps** used out of 50 budget (86%) — close to the ceiling.
- High **read_file count (11)**: model re-read files to find anchors after
  `apply_patch` failures. 10 tool errors across the run; many were
  `apply_patch` rejecting unified-diff anchors before AGENTS.md was updated
  to soften that guidance.
- **apply_patch:write_file = 10:7** — borderline patch-discipline; pulled
  down by reach-for-write_file after apply_patch failures.
- Outcome: implemented `TokenUsage` end-to-end (lib trait + openai +
  agent + CLI). 7 product files changed.

### deepseek-chat — goal 05 (unified-diff tolerance)

- **23 steps** used out of 50 budget (46%) — well under the ceiling.
- Much **leaner tool use overall (23 calls vs 42)**. Model spent less
  time re-discovering the file structure.
- 6 errors, mostly transient `apply_patch` failures while iterating on
  the `normalize_hunk_header` implementation itself.
- **apply_patch:write_file = 6:3** — better surgical-edit ratio than
  MiniMax above, but DeepSeek did fall back to `write_file` to
  reset broken state at least once.
- Outcome: implemented `normalize_hunk_header` plus 9 unified-diff tests
  (4 more than the 5 the goal required — added empty/whitespace/non-unified
  edge cases on its own).

## Caveats

- These are **two data points** in different goals. Don't read "DeepSeek
  is better than MiniMax" out of this yet — goal 04 was a wider-surface
  change (5 files) than goal 05 (1 file), and MiniMax was the first
  attempt against an unforgiving AGENTS.md (the V4A worked example was
  added later in part because of MiniMax's struggles).
- A proper comparison would have both models attempt the same goal.
  The rotation pattern (`RECURSIVE_PROVIDERS=minimax,deepseek`) will
  produce that data set after the next few cycles.
- "err results" counts tool-call results starting with `ERROR:`. Some
  errors are recoverable (model retries with a fix); some are dead
  ends. The metric is noisy; pair it with the verdict.

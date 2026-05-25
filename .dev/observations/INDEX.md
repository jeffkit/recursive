# Observations

Cross-run metrics for self-improve cycles. Auto-extracted from
`.dev/journal/run-*.md` by `.dev/scripts/observe.sh`. Per-run detail
files in this folder; this index is the side-by-side comparison.

## Successful runs

| goal | provider | model | steps | tool calls | err results | apply:write | cost USD | reason |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 04 token-usage | minimax | MiniMax-M2 | 43 | 42 | 10 | 10:7 | (no tracking) | NoMoreToolCalls |
| 05 apply-patch-unified | deepseek | deepseek-chat | 23 | 23 | 6 | 6:3 | (no tracking) | NoMoreToolCalls |
| 06 cost-estimation | minimax | MiniMax-M2 | 29 | 29 | 9 | 9:4 | $0.3831 | NoMoreToolCalls |
| 07 transcript-limit | deepseek | deepseek-chat | 39 | 41 | 9 | 9:4 | $0.4885 | NoMoreToolCalls |
| 08 persistent-transcripts | minimax | MiniMax-M2 | 19 | 19 | 2 | 2:5 | $0.1871 | NoMoreToolCalls |

## Key insight: prompt-token amplification

Goal 06 (MiniMax) consumed **1,240,880 prompt tokens** vs only **9,028
completion tokens** across 29 steps — a **137:1 ratio**. The full
system prompt + accumulated transcript is re-sent on every LLM call.
That means the marginal cost per *agent step* is essentially the
**transcript at that point in time**, not "input + output of one
message".

Practical consequence: a goal that costs $0.38 on MiniMax could be
much cheaper if we trimmed the journal context in the system prompt,
or if we used a cache-aware provider (DeepSeek charges cache-hit at
~10× less). This points squarely at the next observation we want:
re-run the same goal on DeepSeek and compare.

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

### First concurrent batch (07 + 08, both ran in worktrees in parallel)

Goals 07 and 08 launched simultaneously through `parallel-self-improve.sh`,
on disjoint product surface, each in its own git worktree. Wall-clock
≈ 3 min for the pair (vs. estimated 6–8 min serial). No merge conflicts
beyond `src/main.rs`'s CLI struct, which was a trivial 4-block manual
resolution: combine the imports, accept both CLI flags, thread both
through `run_once`'s signature, keep both side effects.

DeepSeek (39 steps, $0.49) was about **2.6× more expensive** than MiniMax
(19 steps, $0.19) on this batch. Some of that is goal size — 07 reaches
deeper into `agent.rs` than 08's mostly green-field module. Some is
genuinely DeepSeek being chattier per step (1.73M prompt vs 0.59M).
Worth re-running the same goal on both providers later to control for it.

### MiniMax-M2 — goal 06 (cost estimation)

- **29 steps** out of 50 budget (58%) — comfortable margin.
- **9 errors** — mostly apply_patch anchor mismatches during the
  middle of the implementation; recovered each time.
- **apply_patch:write_file = 9:4** — better discipline than goal-04
  (10:7) without giving up the four legitimate write_file uses (new
  test file content, full re-edit of `print_usage`).
- **Cost: $0.3831 USD** (prompt 1,240,880 @ \$0.30/M + completion
  9,028 @ \$1.20/M). First cycle where we have a real dollar figure
  to compare against future runs.
- Outcome: shipped `ModelPricing` + `pricing_for` + CLI cost line.
  3 product files changed, 5 new tests, 81 total green.
- Goal phrasing did its job: agent stuck to the listed files
  (`src/llm/mod.rs`, `src/lib.rs`, `src/main.rs`), used `apply_patch`
  for the existing files as instructed, and didn't sprawl.

### deepseek-chat — goal 07 (transcript-limit)

- **39 steps** out of 50 (78%). Larger goal surface (agent.rs +115,
  main.rs +35) explains the higher step count vs. its baseline of 23
  on goal-05.
- **apply_patch:write_file = 9:4** — held discipline through a
  multi-file change. Higher run_shell count (20) suggests the model
  iterated tests aggressively (each `cargo test` is a shell call).
- **Cost: $0.4885** — most expensive run on file. The DeepSeek
  prompt-token bill (1.73M @ $0.27/M) dominated.
- Outcome: `FinishReason::TranscriptLimit { chars, limit }` plus
  `--max-transcript-chars` flag and 3 new tests. CLI also prints
  `note: stopped because transcript reached X chars (limit Y)`.

### MiniMax-M2 — goal 08 (persistent-transcripts)

- **19 steps** out of 50 (38%) — cleanest run so far. Mostly
  green-field (new `src/transcript.rs` module).
- Only **2 errors** the entire run; both transient apply_patch
  anchor mismatches resolved by re-reading the file.
- **apply_patch:write_file = 2:5** — write-heavy because the goal
  explicitly creates a new file. Not a discipline failure.
- **Cost: $0.1871** — cheapest run with cost tracking enabled.
  Short transcript + comparatively small prompt accumulation.
- Outcome: `TranscriptFile`/`TranscriptMeta` types + `--transcript-out`
  flag. Includes a vendored civil-from-days algorithm to avoid pulling
  in `chrono`. 4 new tests including round-trip via a real tempfile.

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

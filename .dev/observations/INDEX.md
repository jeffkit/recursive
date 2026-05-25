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
| 09 transcript-replay | deepseek | deepseek-chat | 18 | 18 | 4 | 4:2 | $0.1437 | NoMoreToolCalls |
| 10 shell-cwd | minimax | MiniMax-M2 | 15 | 14 | 0 | 0:2 | $0.1187 | NoMoreToolCalls |
| 11 search-files-tool | deepseek | deepseek-chat | 29 | 31 | 2 | 13:1 | $0.2425 | NoMoreToolCalls |
| 12 default-system-prompt | minimax | MiniMax-M2 | 15 | 14 | 2 | 2:1 | $0.0804 | NoMoreToolCalls |
| 13 search-regex | deepseek | deepseek-chat | 12 | 12 | 1 | 6:1 | $0.0681 | NoMoreToolCalls |
| 14 json-events (1st) | minimax | MiniMax-M2 | 50 | ~50 | ? | (varied) | (rolled back) | BudgetExceeded |
| 14 json-events (2nd) | deepseek | deepseek-chat | 50 | ~50 | ? | (varied) | $0.0003 | BudgetExceeded (rolled back) |
| 14 json-events (manual) | orchestrator | — | — | — | — | — | $0.0000 | manual landing of DeepSeek's patches |
| 15 retry-policy-config | minimax | MiniMax-M2 | 29 | 29 | 1 | 8:0 | $0.1590 | NoMoreToolCalls |

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

### Second concurrent batch (09 + 10)

- **goal-09 transcript-replay (deepseek)**: 18 steps, 4 errors,
  apply:write=4:2, $0.1437. Dogfood goal-08 by adding
  `recursive replay <file>`. Clean run.
- **goal-10 shell-cwd (minimax)**: 15 steps, **0 errors**,
  apply:write=0:2, $0.1187. Smallest single-file change; agent
  rewrote `shell.rs` whole even though the goal asked for
  `apply_patch` ("Notes for the agent" said use apply_patch). That's
  a recurring MiniMax pattern: when the file is small, it picks
  `write_file` even when patch was suggested.
- Wall-clock for the pair ≈ 1.5 min — faster than the first
  concurrent batch because both goals were smaller. No merge
  conflicts at all this time (09 changed main.rs + transcript.rs,
  10 changed only tools/shell.rs).
- Pair total cost: **$0.26**. Cheapest two-goal batch on record.

### Third concurrent batch (11 + 12)

- **goal-11 search-files-tool (deepseek)**: 29 steps, 2 errors,
  **apply:write = 13:1** (best discipline ratio yet), $0.2425.
  Added a new `SearchFiles` tool with substring search, capped +
  sandboxed, plus `walkdir` dep. 5 new tests.
- **goal-12 default-system-prompt (minimax)**: 15 steps, 2 errors,
  apply:write = 2:1 (much better than goal-10's 0:2), **$0.0804 —
  cheapest run on record**. Replaced the minimal default system
  prompt with an opinionated short version pointing at apply_patch
  + post-change tests + anti-stuck guidance. 3 new tests.
- **Zero merge conflicts**. Surface was actually disjoint this time
  (11 = src/tools/* + Cargo.toml + main.rs build_tools, 12 = only
  src/config.rs). Pair total **$0.32**.
- Note: the goal-12 fix may itself improve future goal-10-style
  small-file runs because the new default prompt now explicitly
  says "prefer apply_patch over write_file when modifying existing
  files". The next MiniMax small-file run will tell us if the nudge
  works.

### Fourth concurrent batch (13 + 14 — first rollback)

- **goal-13 search-regex (deepseek)**: 12 steps, 1 error, **\$0.0681
  — new cheapest record**. Built incrementally on its own
  goal-11 work (DeepSeek doing follow-up extensions to its own
  prior contribution is the most efficient pattern observed).
- **goal-14 json-events (minimax)**: **rolled back** at step 50
  (BudgetExceeded). MiniMax kept making *correct* surgical edits but
  the goal touched too many places in `main.rs` (run_once signature,
  repl signature, dispatcher, output suppression in two places).
  Each edit's tool result extended the transcript; the next LLM call
  re-sent everything; the budget ran out before the final
  `cargo test`. Failure mode is "death by surface area", not "agent
  confusion".
- Lesson encoded: when a goal's "Scope" lists 4+ edits in one file,
  prefer DeepSeek (better step economy) over MiniMax. Or split into
  two smaller goals.
- Recovery: goal-14 is being rerun on DeepSeek in batch 5. The
  rolled-back journal commit is cherry-picked to main for the
  diagnostic record.

### Fifth concurrent batch (14 retry + 15 — second rollback, manual landing)

- **goal-15 retry-policy-config (minimax)**: 29 steps, 1 error,
  **apply:write = 8:0 (perfect discipline)**, $0.1590. The opinionated
  default system prompt from goal-12 is doing its job — MiniMax now
  *never* falls back to `write_file` on existing files. Two new tests
  for default + env-override behavior. Outcome: `RECURSIVE_RETRY_MAX`,
  `RECURSIVE_RETRY_INITIAL_BACKOFF_SECS`, `RECURSIVE_RETRY_MAX_BACKOFF_SECS`
  are honored by `Config::from_env` and threaded into `OpenAiProvider`.
- **goal-14 json-events (deepseek, 2nd attempt)**: **also rolled back**
  at step 50 (BudgetExceeded). The product code was *correct* — the
  agent's JSON output `{"kind":"usage", …}` was the exact shape goal-14
  required — but the agent burned its late steps trying to verify with
  `cargo run … | jq` on a fresh worktree, where the first `cargo build`'s
  `Compiling …` lines polluted stdout enough to break jq. Two consecutive
  rollbacks on the same goal, both **for the same verification-path
  reason, not a product-code reason**.
- **Recovery: manual landing.** Per SOP §6 (two rollbacks → human
  intervention), but in this case the diagnosis is unambiguous and the
  patches are recoverable from the run log. I hand-applied DeepSeek's
  7 patches plus 4 new serialization unit tests. 113 tests pass,
  clippy clean.
- **Lesson encoded in `.dev/AGENTS.md`:** "Verify behavior through
  `cargo test`, never through `cargo run | jq`. On a fresh worktree
  `cargo run` emits `Compiling …` lines that break jq parsing and
  burn your step budget. If you need to assert on JSON shape, write a
  unit test."
- Batch 5 pair total cost: **$0.16 spent + manual diagnostic time
  saved $0.15** vs a 3rd LLM attempt on goal-14.

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

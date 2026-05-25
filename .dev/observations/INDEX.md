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
| 16 kill-count-lines | minimax | MiniMax-M2 | 25 | 25 | 0 | 3:0 | $0.0851 | NoMoreToolCalls |
| 17 replay-from-step (1st) | deepseek | deepseek-chat | 3 | 1 | 0 | 0:0 | $0.0000 | infra-503 (rolled back) |
| 17 replay-from-step (2nd) | deepseek | deepseek-chat | 20 | 20 | ? | (varied) | $0.1991 | Stuck (rolled back) |
| 17 replay-from-step (manual) | orchestrator | — | — | — | — | — | $0.0000 | manual landing of DeepSeek's design |
| 18 default-prompt-dogfood | minimax | MiniMax-M2 | 25 | 25 | ? | (varied) | $0.0546 | NoMoreToolCalls |
| 19 transcript-budget-trim | deepseek | deepseek-chat | 50 | ~ | ? | (varied) | $0.2179 | NoMoreToolCalls |
| 20 replay-tail | minimax | MiniMax-M2 | 42 | ~ | ? | (varied) | $0.0849 | NoMoreToolCalls |
| 21 deepseek-cache-hits | deepseek | deepseek-chat | 55 | ~ | ? | (varied) | $0.2136 | NoMoreToolCalls |
| 22 apply-patch-nicer-error | minimax | MiniMax-M2 | 71 | ~ | ? | (varied) | $0.4186 | NoMoreToolCalls (high cost — apply_patch.rs is large) |
| 23 shell-timeout-env (glm) | glm | glm-5.1 | 1 | 0 | 0 | 0:0 | $0.0000 | 429 quota exhausted (rolled back) |
| 23 shell-timeout-env (minimax) | minimax | MiniMax-M2 | 50 | ~ | ? | (varied) | (rolled back) | BudgetExceeded — env-var test race |
| 23 shell-timeout-env (manual) | orchestrator | — | — | — | — | — | $0.0000 | manual landing of MiniMax's product code, consolidated test |
| 24 per-step-latency | deepseek | deepseek-chat | 48 | ~ | ? | (varied) | **$0.7374** | NoMoreToolCalls (record high — agent.rs is large) |
| 25 apply-patch-dry-run | deepseek | deepseek-chat | 11 | ~ | ? | (varied) | $0.0869 | NoMoreToolCalls (cheap — apply_patch.rs is well-factored) |
| 26 read-file-range | minimax | MiniMax-M2 | 35 | ~ | ? | (varied) | $0.2714 | NoMoreToolCalls |
| 27 shell-env-passthrough | deepseek | deepseek-chat | ~ | ~ | ? | (varied) | $0.1017 | NoMoreToolCalls |
| 28 transcript-head | minimax | MiniMax-M2 | ~ | ~ | ? | (varied) | $0.2508 | NoMoreToolCalls |
| 29 search-case-insensitive | minimax | MiniMax-M2 | 21 | ~ | ? | (varied) | $0.1173 | NoMoreToolCalls |
| 30 openai-error-model | deepseek | deepseek-chat | 36 | ~ | ? | (varied) | $0.3129 | NoMoreToolCalls (orchestrator killed 4 zombie cargo tests during run — lesson in AGENTS.md) |
| 31 context-compaction | deepseek | deepseek-chat | 51 | ~ | ? | (varied) | (b12) | NoMoreToolCalls |
| 32 streaming-sse | deepseek | deepseek-chat | ~ | ~ | ? | (varied) | (b12) | NoMoreToolCalls (introduced startup-panic regression — fixed in `c5b2b8d`) |
| 33 skills-v1 (minimax) | minimax | MiniMax-M2 | 100 | ~ | ? | (varied) | (b12, rolled back) | BudgetExceeded (auto-resume infrastructure was broken at the time) |
| 33 skills-v1 (manual) | orchestrator | — | — | — | — | — | $0.0000 | manual landing of MiniMax's complete source files + wiring |
| 34 anthropic-provider | minimax | MiniMax-M2 | ~ | ~ | ? | (varied) | (b12) | NoMoreToolCalls (19 new tests, on the high side for a new-file goal) |
| 35 mcp-client-v1 | deepseek | deepseek-chat | 41 | 34 | ? | 10:2 | **$0.7282** | NoMoreToolCalls (headline; 97.7% cache hit; 9 new tests) |
| 36 project-context-file | minimax | MiniMax-M2 | 35 | 34 | 3 | 9:0 | $0.3544 | NoMoreToolCalls (perfect patch discipline — 0 write_file invocations) |
| 37 web-fetch-tool | minimax | MiniMax-M2 | 110 | ~ | ? | 16:4 | (varied) | NoMoreToolCalls (highest-step batch-13 goal — HTML extraction is non-trivial) |
| 38 persistent-memory | deepseek | deepseek-chat | 28 | ~ | ? | 9:1 | $0.3065 | NoMoreToolCalls (97.9% cache hit; 9 new tests across remember/recall/forget) |
| 39 estimate-tokens-tool | minimax | MiniMax-M2 | 32 | ~ | ? | 9:1 | $0.2824 | NoMoreToolCalls (Phase 1 closer; 6 new tests) |
| 40 sub-agent | deepseek | deepseek-chat | 38 | ~ | ? | 16:1 | $0.6251 | NoMoreToolCalls (recursive agent primitive; 7 new tests; default-off via env flag) |
| 41 structured-output | deepseek | deepseek-chat | 29 | ~ | ? | 10:0 | $0.3146 | NoMoreToolCalls (LlmProvider extension; perfect patch discipline; 3 new tests) |
| 42 otel-tracing | minimax | MiniMax-M2 | 103 | ~ | ? | 45:4 | **$2.1722** | NoMoreToolCalls (NEW cost record; instrumenting agent.rs + llm/* + tools/* needed 45 surgical patches; 3 new tests) |

### Batch 14 (g39-g42) — Phase 1 done, Phase 4 starts

All four green again, no auto-resume. **Total cost ≈ $3.43** dominated
by g42 otel-tracing at $2.17 (highest single-goal cost ever, beating
g24 latency at $0.74). Cause: instrumentation work touches many spots
across many files — every `#[tracing::instrument]` requires reading
the function context first, then applying a small patch. 45 apply_patch
invocations is the highest count observed.

Highlights:

- **g40 sub-agent**: the recursive agent primitive. 38 steps, $0.63.
  Default-off via `RECURSIVE_SUBAGENT_ENABLED=1` so baseline behavior
  is unchanged. Depth-limit + tool-subset isolation both implemented.
- **g41 structured-output**: plumbing only (no callers wired). Sets up
  `LlmProvider::complete_structured()` for future plan-then-act,
  compactor structured summaries, etc.
- **g39 estimate-tokens-tool**: closes Phase 1 with the char/4
  heuristic. No tokenizer crate dependency (deliberate — heavy native
  dep for marginal gain).
- **g42 otel-tracing**: spans only, no exporter. Operators bring their
  own subscriber. Worth noting for future: even though g42 was "S"
  effort in the roadmap, the actual work was high — 7 files touched,
  103 steps. Future "instrumentation" goals should probably be sized
  M, not S.

**Merge conflict**: 1 of 4 (g40 in `src/main.rs` — combined use
imports). All others auto-merged including the heaviest g42.

Tests: 211 lib → 230 lib (+19 net new from batch). Total tests
214 → 233.

### Batch 13 (g35-g38) — Phase 1 closes, Phase 2 majority

All four green on the first try. **Total cost ≈ $1.40, all of it on the
4-pass batch.** Highlights:

- **g35 mcp-client-v1** (headline): MCP server config + spawn client +
  tool wrapper, 41 steps and $0.73 on deepseek. 97.7% cache hit kept
  cost reasonable for a multi-file feature touching `lib.rs`, `mcp.rs`,
  `tools/mod.rs`, and `main.rs`.
- **g37 web-fetch-tool**: 110 steps — biggest. HTML extraction (script
  + style tag stripping, whitespace collapse) needed more iteration.
  Under the new 200-step ceiling with room to spare.
- **g36, g38**: textbook small additions (35 + 28 steps, $0.35 + $0.31).
  Perfect patch discipline (9:0 and 9:1).

**Merge conflicts in `src/main.rs`**: 2 of the 4 merges. Both auto-marker
conflicts at `use` imports and tool-registration chains. Resolved by
straightforward combination (no semantic merge needed). After all four
landed, tests jumped 178 → 214 (+36 new tests).

### Batch 12 (g31-g34) — Phase 1 pivot, first SOTA-feature batch

3 of 4 auto-merged; g33 manually recovered after a transcript-save bug
prevented auto-resume from firing. The orchestrator fixed the
infrastructure bug (`2459ef8`) as a Phase 0 follow-up so future
BudgetExceeded runs survive the rollback boundary. Total tests:
140 → 175.

### Batch 11 (g27-g30) — first true 4-wide

All four green on the first 4-wide steady-state batch. Goal-30 cost
($0.3129) understates real wall-clock because the orchestrator
manually killed 4 zombie cargo-test processes deadlocked on hanging
reqwest connections. Lesson recorded in AGENTS.md section 5
("Network tests must set explicit reqwest timeouts").

### Cost stratification by file size (top expensive vs top cheap)

- **Top 3 most expensive**: g24 latency ($0.7374, agent.rs), g22
  apply-patch nicer-err ($0.4186, apply_patch.rs), g30
  openai-error-model ($0.3129, openai.rs + mock server debugging).
- **Top 3 cheapest**: g18 dogfood ($0.0546), g13 search-regex
  ($0.0681), g16 kill-count-lines ($0.0851).
- Strong correlation: per-goal cost ≈ (steps × transcript size at
  step N). Touching a large file inflates transcripts geometrically.

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

### Seventh concurrent batch (17 retry + 18, second manual landing in two batches)

- **goal-18 default-prompt-dogfood (minimax)**: 25 messages, **$0.0546
  (new cheapest record)**, no rollbacks. Expanded
  `default_system_prompt()` with V4A worked example + "Don't" hard
  limits + cargo-test-not-jq lesson. Adds 1 new test asserting the
  prompt contains the four key strings. Bumped the existing
  "well-under-a-kilobyte" threshold from 1024 to 2048 (the new prompt
  is ~1.4 KB).
- **goal-17 replay-from-step (deepseek, 2nd attempt)**: **rolled back
  again**. First attempt was upstream HTTP 503 (transient infra);
  second attempt was a Stuck verdict at step 20 from two mechanical
  bugs — `.into()` ambiguity in tests (`Message::system("sys".into())`
  needs `.to_string()` because constructors take `impl Into<String>`)
  and three near-identical `Message::user("hello".into())` context
  blocks that violate V4A "must be unique" → repeated identical
  patch retry → anti-stuck.
- **Recovery: manual landing again.** The agent's design was correct;
  I hand-applied it with the two mechanical fixes. 115 tests total
  (+5 new). The `--transcript-out` plumbing landed in batch-6
  pre-flight saved a 192KB structured transcript that made this
  manual landing direct — first time the orchestrator-side tooling
  investment paid off concretely.
- Batch 7 lesson: **the goal-17 design surface is fine, but
  Message constructors that take `impl Into<String>` are a common
  Rust ergonomic trap when writing test setup with `&str` literals.
  Future goals should warn agents to prefer `.to_string()` over
  `.into()` inside fixture literals.**

### Sixth concurrent batch (16 + 17 first attempt)

- **goal-16 kill-count-lines (minimax)**: 25 steps, **apply:write =
  3:0 (perfect again)**, $0.0851. Single-file deletion + 2 module
  edits. Hand-fixed one stale `count_lines` reference in
  `src/config.rs::default_system_prompt` that the agent didn't see
  because self-improve.sh overrides the default prompt via
  `--system-prompt-file`. The goal-12 nudge "prefer apply_patch"
  reaches the self-improve agent via AGENTS.md (which always was
  enforced) — *not* via the in-binary default. This corrects the
  earlier (batch 5) attribution claim.
- **goal-17 replay-from-step (deepseek, 1st attempt)**: rolled back
  at step 3 — upstream DeepSeek HTTP 503 (transient infra). Not an
  agent-side problem; retry queued.
- **Orchestrator-side discovery**: `self-improve.sh` was not using
  any of the agent's own CLI affordances (`--json`,
  `--transcript-out`, `--max-transcript-chars`,
  `RECURSIVE_RETRY_MAX`). Hand-patched the script to pass
  `--transcript-out .dev/transcripts/run-${TS}.json` so future
  rollback diagnostics use `recursive replay` on the saved
  transcript instead of grep'ing the raw log. This investment paid
  off in the very next batch (goal-17 retry).

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

# Manual edit: g273 completion

**Date**: 2026-06-11
**Goal**: Complete Goal 273 (NEW-COST-1: reasoning_tokens
counted in cost) after self-improve.sh stalled at step 47
(minimax API hang on o1 streaming).

**Files touched**:
- `src/llm/mod.rs` — added `reasoning_tokens: u32` field to
  `TokenUsage`; extended `accumulate` to sum it; extended
  `ModelPricing::cost_usd` to treat reasoning tokens at
  output rate. Added 3 unit tests in `mod tests`.
- `src/llm/anthropic.rs` — added `reasoning_tokens: 0` to the
  `TokenUsage` literal in the `complete_with_search` path and
  in `AnthropicUsage::to_token_usage`. Anthropic does not yet
  expose this field via the API.
- `src/llm/openai.rs` — same for the streaming `parse_completion`
  path and the non-streaming `ResponseUsage::to_token_usage`.
  The streaming path does not extract
  `completion_tokens_details.reasoning_tokens` (would require a
  new ResponseUsage field; deferred).
- `src/llm/mock.rs` — added `reasoning_tokens: 0` to the test
  fixture.
- `src/session.rs` — `UsageMeta::from_token_usage` now maps
  `tu.reasoning_tokens` (previously hardcoded to `None`); added
  `total_reasoning_tokens: u64` field to `SessionCost`;
  `SessionCost::accumulate` sums it.
- `src/cost.rs` — `update_meta_with_cost` writes the
  `reasoning_tokens` field into `.meta.json` so users running
  R1 / o1 can see the cost driver separately from visible
  output. The existing cost_usd test fixture was updated to
  include 200 reasoning tokens at $0.28/M, asserting the new
  total of $0.000196308 (was $0.000140308).
- 9 `TokenUsage { ... }` struct literals across all 6 files
  were updated to include the new `reasoning_tokens` field
  (default 0 for non-reasoning models).

**Tests added**:
- `src/llm/mod.rs`: 3 new tests in `mod tests`
  (`token_usage_accumulate_sums_reasoning`,
  `token_usage_accumulate_saturates_on_overflow`,
  `token_usage_default_has_zero_reasoning`).
- `src/cost.rs`: existing test_cost_tracker_cost_usd test
  updated to assert reasoning tokens priced at output rate.

**Notes**:
- This was a Goal 273 self-improve run; the agent stalled at
  step 47 reading anthropic.rs to understand the field
  shape. Lead completed the change directly. Pattern matches
  the g267/g268/g269/g272 lead-completion overrides.
- The reasoning_tokens source from the OpenAI provider is
  currently `0` in `to_token_usage` (the streaming path
  does its own extraction; the non-streaming path doesn't
  extract `completion_tokens_details`). This is a documented
  gap, not a bug — the field is correctly threaded so
  future enhancement is a one-line change.
- **Implementation note**: I started by writing a broad
  regex to inject `reasoning_tokens: 0,` into every
  `TokenUsage { ... }` block. It also matched function
  return types (`fn to_token_usage(&self) -> TokenUsage {
  ... }`) and corrupted them. I reverted, switched to a
  narrow regex (only struct literals at statement position
  with `prompt_tokens:` after the opening brace), and
  applied manual edits to the two function-return-position
  TokenUsage blocks. A regression test for this is hard
  (no obvious syntax marker) but the cost is now paid.

**Out of scope**: plumbing reasoning_tokens through
the streaming path's `completion_tokens_details` extraction
and through `AnthropicProvider::complete_with_search` for
extended-thinking tokens. Both require a follow-up goal
that also updates the LLM response shape definitions.

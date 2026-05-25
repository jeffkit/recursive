# Goal 49 — Cache-Aware Cost Estimation

**Roadmap**: D.2 — Cache-Aware Cost Estimation

**Design principle check**:
- Implemented as: extension to existing cost calculation helper in
  `src/llm/mod.rs`; observer / CLI summary only.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

Goal 21 added `cache_hit_tokens` / `cache_miss_tokens` to `TokenUsage`,
but `ModelPricing::cost_usd()` still bills ALL prompt tokens at the full
input rate. DeepSeek runs routinely show 95%+ cache hit — the printed
cost over-estimates by ~10x vs actual billing. This makes the cost signal
in `observe.sh` output unreliable for orchestrator decisions.

## Scope (do exactly this, no more)

### 1. `src/llm/mod.rs` — extend `ModelPricing` and `cost_usd()`

Current signature (approx):
```rust
pub fn cost_usd(&self, usage: &TokenUsage) -> f64
```

Change the calculation:
- If `usage.cache_hit_tokens > 0`:
  - `cache_hit_cost = cache_hit_tokens * self.cache_hit_input_per_token`
  - `cache_miss_cost = (prompt_tokens - cache_hit_tokens) * self.input_per_token`
  - Total input cost = `cache_hit_cost + cache_miss_cost`
- Else: fall back to current behavior (`prompt_tokens * input_per_token`)
- Output cost unchanged: `completion_tokens * output_per_token`

Add a `cache_hit_input_per_million: f64` field to `ModelPricing` with a
sensible default (e.g. `input_per_million * 0.1` for DeepSeek's known 10x
discount, `input_per_million * 0.9` as a conservative default for unknown
models).

### 2. Update `pricing_for()` entries

For models already in the match table:
- `deepseek-chat` / `deepseek-v4-*`: cache hit = 10% of input rate
- MiniMax: no known cache pricing, use `input_per_million` (no discount)
- Others: default to full rate (conservative)

### 3. `print_usage` in `src/main.rs`

The cost line already prints `cost: $X.XXXX`. No change needed IF
`cost_usd()` is already called on accumulated usage. Just verify it
passes through correctly. If the accumulated `TokenUsage` properly sums
`cache_hit_tokens`, the fix is self-contained in `cost_usd()`.

### 4. Tests

- Test: `cost_usd` with `cache_hit_tokens = 0` returns same as before
  (backward compat)
- Test: `cost_usd` with `cache_hit_tokens = 900, prompt_tokens = 1000`
  for a DeepSeek model returns correct discounted cost
- Test: `cost_usd` with unknown model uses conservative default
- Test: accumulated `TokenUsage` preserves `cache_hit_tokens` sum

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- Cost for a run with 97% cache hit is now ~10x lower than before
  (matching actual billing)
- No new dependencies
- No changes to `agent.rs`

## Notes for the agent

- Read `src/llm/mod.rs` — find `ModelPricing`, `pricing_for()`, and
  `cost_usd()`. The struct and helpers are straightforward.
- `TokenUsage` has `cache_hit_tokens` and `cache_miss_tokens` fields
  (added in goal-21). Verify they're populated by grepping for them.
- This is a pure calculation fix. Don't touch `agent.rs`, `config.rs`,
  or any tool files.
- If `pricing_for()` returns `None` for unknown models, make sure the
  cache discount gracefully degrades (use full input rate).

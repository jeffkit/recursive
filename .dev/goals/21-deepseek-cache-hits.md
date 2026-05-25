# Goal 21 — Track DeepSeek prompt cache hit/miss tokens

## Why

DeepSeek's chat completion API returns `usage.prompt_cache_hit_tokens`
and `usage.prompt_cache_miss_tokens` alongside the regular
`prompt_tokens` / `completion_tokens` fields. Cache-hit tokens are
billed at ~10× discount (in our case $0.027/M instead of $0.27/M).

Observation INDEX.md identified this as the **highest-leverage cost
win** because our prompt-token amplification is 137:1 over completion
tokens — most of our spend is on resending the same system prompt and
transcript prefix that the server has already cached.

Today we ignore those fields entirely. We just sum `prompt_tokens` and
`completion_tokens` into `TokenUsage`. Result: we have no visibility
into whether our system-prompt + transcript shape is actually
cache-friendly, and our cost-estimation function over-bills DeepSeek
runs whose prompts were mostly cached.

## Scope

Touches: `src/llm/mod.rs` and `src/llm/openai.rs`.

1. **Extend `TokenUsage`** in `src/llm/mod.rs`:
   - Add two new `usize` fields: `cache_hit_tokens` and
     `cache_miss_tokens`. Both default to 0. Both serialize/
     deserialize like the existing fields (TokenUsage already
     derives Serialize / Deserialize).
   - Update the `accumulate(self, other)` method to sum both new
     fields too.
   - **Do not** change the existing `cost_usd` method on
     `ModelPricing` to account for cache pricing yet — that's a
     separate refinement. This goal is *visibility only*; we just
     surface the numbers so the next goal can act on them.

2. **Extend `OpenAiProvider`'s JSON response parsing** in
   `src/llm/openai.rs`:
   - Where the existing code reads `usage.prompt_tokens` /
     `usage.completion_tokens` / `usage.total_tokens`, also read the
     optional `usage.prompt_cache_hit_tokens` and
     `usage.prompt_cache_miss_tokens`. Use `.unwrap_or(0)` for both
     — they're absent on non-DeepSeek providers.

3. **Update `print_usage` in `src/main.rs`** to additionally print
   the cache line when `cache_hit_tokens > 0`:
   ```
   cache: hit=12345 miss=678 (97.3% hit rate)
   ```

4. **Tests**:
   - In `src/llm/mod.rs`, a test that constructs two `TokenUsage`
     values with cache fields set and calls `accumulate`, asserts
     the cache fields sum correctly.
   - In `src/llm/openai.rs` (or wherever the response parsing test
     lives), if there's already a test that parses a sample usage
     blob, extend it. Otherwise add a small test that builds a
     `serde_json::Value` containing the new fields and verifies the
     parser picks them up.

## Acceptance

- `cargo build` green.
- `cargo test` green (119 baseline + ≥2 new = 121).
- `cargo clippy --all-targets -- -D warnings` green.
- `cargo fmt --all` clean.

## Notes for the agent

- This is **observability only** — no cost-calculation changes.
- Use `apply_patch`. The TokenUsage struct change is the central
  one; the rest is plumbing.
- `print_usage` lives in `src/main.rs` near the top of the
  helpers (`fn print_usage(usage: TokenUsage, model: &str)`).
- **In tests, prefer `.to_string()` over `.into()` for string
  literals** — see AGENTS.md section 5 trap note.
- This goal should be a 10–15 step run.

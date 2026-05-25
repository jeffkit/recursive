# Goal 06 — Cost estimation from token usage

## What

Convert `TokenUsage` into an estimated USD cost. Surface that cost in
`AgentOutcome` and in the CLI's summary line, so a developer can see at
a glance what a run cost.

## Why

Goal 04 added `TokenUsage` (prompt/completion/total tokens) to every
LLM call and accumulated it into `AgentOutcome.total_usage`. That's
half the story. What we actually care about for running a self-improve
loop on real APIs is **how much each run cost in dollars**, so we can
budget cycles and compare providers fairly. Right now we'd have to
multiply tokens by per-model pricing in our head every time.

## Scope (do exactly this, no more)

### 1. `src/llm/mod.rs`

Add a `ModelPricing` value type with per-million-token rates and a
helper that converts a `TokenUsage` into a cost:

```rust
/// Per-million-token pricing for one model. USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

impl ModelPricing {
    /// USD cost for the given usage at this pricing.
    pub fn cost_usd(&self, usage: TokenUsage) -> f64 {
        let in_cost  = (usage.prompt_tokens     as f64) * self.input_per_million  / 1_000_000.0;
        let out_cost = (usage.completion_tokens as f64) * self.output_per_million / 1_000_000.0;
        in_cost + out_cost
    }
}
```

Add a lookup function from model name to `Option<ModelPricing>`. Cover
the models we actually run:

```rust
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    match model {
        "MiniMax-M2"      => Some(ModelPricing { input_per_million: 0.30, output_per_million: 1.20 }),
        "deepseek-chat"   => Some(ModelPricing { input_per_million: 0.27, output_per_million: 1.10 }),
        "glm-4-flash"     => Some(ModelPricing { input_per_million: 0.10, output_per_million: 0.10 }),
        _ => None,
    }
}
```

These numbers are approximate cache-miss list prices as of late 2025.
Don't research them further — they're meant to be tunable, not
authoritative. The goal is the *plumbing*, not the price table.

Re-export `ModelPricing` and `pricing_for` from `src/lib.rs`.

### 2. CLI summary

In `src/main.rs`, after the existing `tokens: prompt=X completion=Y total=Z`
line, print a cost line **only if** we have pricing for the active model:

```
cost: $0.0123 (MiniMax-M2)
```

Print to stderr alongside the existing token summary, in both `run` and
`repl` commands. If `pricing_for` returns `None`, skip the line silently
(no warning — unknown model isn't an error).

### 3. Tests

Add unit tests in `src/llm/mod.rs`:

1. `cost_usd_handles_zero_usage` — empty `TokenUsage` → 0.0
2. `cost_usd_computes_simple_case` — 1M input + 0 output at $1.0/M → 1.0
3. `cost_usd_mixes_input_and_output` — non-trivial mix
4. `pricing_for_known_models` — MiniMax-M2 + deepseek-chat both return Some
5. `pricing_for_unknown_returns_none` — random string → None

Use `(a - b).abs() < 1e-9` for float comparison.

## Out of scope

- Persisting cost history anywhere.
- Per-step cost in `StepEvent` (only the aggregate cost matters for now).
- Cache-hit/cache-miss differentiated pricing (DeepSeek splits these;
  we don't track which is which).
- Configurable price table from env or file — hardcoded is fine for now.
- Touching `OpenAiProvider` — pricing is a pure value-type concern.

## Definition of done

- `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` all
  green.
- Running `recursive run "hi"` (with an env-configured provider that
  returns usage) prints both the `tokens:` line and the `cost: $X.XXXX`
  line on stderr.
- All 5 new tests pass.
- No new TODO comments, no new dependencies.

## Notes for the agent

- This is a **value-types-only** feature. Don't touch `LlmProvider`
  trait. Don't touch `Agent`. Just `src/llm/mod.rs`, `src/lib.rs`,
  `src/main.rs`.
- Use `apply_patch` for the edits in `src/llm/mod.rs` and
  `src/main.rs` — both files already exist and you're adding small
  chunks. `write_file` would be wasteful here.
- For the CLI line, use `{:.4}` precision — 4 decimal places of a
  dollar is enough resolution for typical run costs ($0.0001).

# Goal 51 — External Pricing Table

**Roadmap**: D.1 — External Pricing Table

**Design principle check**:
- Implemented as: new `.dev/pricing.yaml` config file + CLI/library
  cost helper reads from it; dev-infra / observer.
- Does NOT branch inside `agent.rs::Agent::run`'s main loop.

## Why

Goal 06 hard-coded per-model USD rates in `pricing_for()` inside
`src/llm/mod.rs`. That was fine for plumbing, but prices drift (V4
Flash/Pro, cache tiers, new providers) and every update requires a product
commit. Goal 49 just added cache-hit pricing to the same struct — now is
the right time to externalize the whole table.

The price table is **orchestrator configuration**, not kernel logic.
Moving it out keeps the binary slim and lets the orchestrator update
rates without recompiling.

## Scope (do exactly this, no more)

### 1. `.dev/pricing.yaml` — new config file

```yaml
# Model pricing table (USD per million tokens)
# Used by: recursive CLI (--pricing-file), observe.sh
models:
  deepseek-chat:
    input_per_million: 0.27
    output_per_million: 1.10
    cache_hit_input_per_million: 0.027  # 10% of input
  deepseek-v4-flash:
    input_per_million: 0.27
    output_per_million: 1.10
    cache_hit_input_per_million: 0.027
  MiniMax-M2:
    input_per_million: 1.00
    output_per_million: 9.00
    cache_hit_input_per_million: 1.00  # no known discount
  glm-5.1:
    input_per_million: 0.50
    output_per_million: 1.00
    cache_hit_input_per_million: 0.50
```

### 2. `src/llm/mod.rs` — load external pricing

Add a function:
```rust
pub fn load_pricing_from_yaml(path: &Path) -> Result<HashMap<String, ModelPricing>>
```

That reads the YAML file and returns a map of model name → `ModelPricing`.
Use `serde_yaml` or manual parsing (prefer manual YAML parsing with
existing deps if possible to avoid new dependency — or justify adding
`serde_yaml`).

**Fallback**: `pricing_for()` keeps its hardcoded table as the default.
If an external file is loaded, it takes precedence. Models not in the
external file fall back to hardcoded or `None`.

### 3. `src/main.rs` — CLI flag + env

Add `--pricing-file <path>` flag (or `RECURSIVE_PRICING_FILE` env var):
- When set, loads the external pricing table
- `print_usage()` uses external pricing if available, else fallback

### 4. Tests

- Test: `load_pricing_from_yaml` parses the sample YAML correctly
- Test: External pricing overrides hardcoded values
- Test: Missing model in external file falls back to hardcoded
- Test: Malformed YAML returns descriptive error

## Acceptance

- `cargo test` green
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `.dev/pricing.yaml` committed with current rates
- `recursive run --pricing-file .dev/pricing.yaml "..."` uses external rates
- No breaking changes to existing behavior when flag is not set
- **Dependency decision**: if adding `serde_yaml`, justify in commit msg.
  Alternative: parse simple flat YAML manually with regex (model names
  are simple strings, values are floats — this is viable).

## Notes for the agent

- The existing `pricing_for(model: &str) -> Option<ModelPricing>` is a
  match statement in `src/llm/mod.rs`. Keep it as fallback.
- For YAML parsing without `serde_yaml`: the format is flat enough that
  a line-by-line parser would work (indented key: value). But `serde_yaml`
  is a standard dep and justified here. Your call.
- Don't change `cost_usd()` logic — g49 just fixed it. Only change WHERE
  the `ModelPricing` struct comes from (external file vs hardcoded match).
- The file path `.dev/pricing.yaml` is a convention for this repo. The CLI
  flag accepts any path — it's the user's choice where to put it.

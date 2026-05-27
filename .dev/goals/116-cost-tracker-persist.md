# Goal 116 — Cost tracker: persist per-run cost summary to session

**Roadmap**: Phase 15.3 — Observability / Cost Tracking (part 1/2)

**Design principle check**:
- Implemented as: `CostSummary` struct written to session dir on run completion
- Reads `AgentOutcome.total_usage` + pricing info → computes cost → writes JSON
- ❌ Does NOT modify agent.rs or the run loop

## Why

Every run computes token usage, but the cost is only visible in the
agent's stderr output. For the evaluation system, we need machine-readable
cost data persisted alongside the session. This writes a `cost.json` file
to the session directory after each run.

## Scope (do exactly this, no more)

### 1. Create `src/cost.rs` module

```rust
use crate::llm::{ModelPricing, TokenUsage};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    pub model: String,
    pub tokens_prompt: u32,
    pub tokens_completion: u32,
    pub tokens_cache_hit: u32,
    pub cost_usd: f64,
    pub steps: usize,
    pub llm_latency_ms: u64,
}

impl CostSummary {
    pub fn from_usage(
        model: &str,
        usage: TokenUsage,
        pricing: Option<ModelPricing>,
        steps: usize,
        llm_latency_ms: u64,
    ) -> Self {
        let cost_usd = pricing
            .map(|p| p.cost_usd(usage))
            .unwrap_or(0.0);
        Self {
            model: model.to_string(),
            tokens_prompt: usage.prompt_tokens,
            tokens_completion: usage.completion_tokens,
            tokens_cache_hit: usage.cache_hit_tokens,
            cost_usd,
            steps,
            llm_latency_ms,
        }
    }

    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }
}
```

### 2. Wire into `src/main.rs` after agent.run()

After `agent.run()` returns, compute and persist cost:

```rust
// After agent.run() returns the outcome
let cost = CostSummary::from_usage(
    &config.model,
    outcome.total_usage,
    effective_pricing,  // from the existing pricing lookup
    outcome.steps,
    outcome.total_llm_latency_ms,
);
if let Some(ref w) = session_writer {
    let session_dir = w.lock().unwrap().session_dir().to_path_buf();
    let cost_path = session_dir.join("cost.json");
    let _ = cost.write_to(&cost_path);  // fire-and-forget
}
// Also print to stderr for human visibility
eprintln!("cost: ${:.4} ({} prompt + {} completion tokens)",
    cost.cost_usd, cost.tokens_prompt, cost.tokens_completion);
```

### 3. Register module in `src/lib.rs`

Add `pub mod cost;` to src/lib.rs.

### 4. Tests

- **Test A**: `CostSummary::from_usage` computes cost correctly
- **Test B**: `CostSummary::from_usage` with None pricing returns cost 0.0
- **Test C**: `write_to` produces valid JSON file
- **Test D**: JSON round-trips (write + read back)

## Acceptance

- `cargo build` green.
- `cargo test` green.
- `cargo clippy --all-targets -- -D warnings` green.
- Running a session produces `cost.json` in the session dir.
- `cost.json` contains correct token counts and computed USD cost.

## Notes for the agent

- `serde` and `serde_json` are already in Cargo.toml.
- The `ModelPricing` struct and `pricing_for()` are in `src/llm/mod.rs`.
  Import them with `use crate::llm::{ModelPricing, TokenUsage, pricing_for}`.
- The effective pricing in main.rs is resolved by the existing
  `get_effective_pricing()` helper — look for it and reuse.
- The session directory path comes from `session_writer.session_dir()`.
  Only write cost.json if a session is active.
- Do NOT modify `src/agent.rs`. All the data you need is in `AgentOutcome`.
- Files to create: `src/cost.rs`
- Files to modify: `src/lib.rs` (1 line), `src/main.rs` (~10 lines)

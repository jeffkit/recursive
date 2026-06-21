//! Token usage accounting and cost estimation for the Recursive TUI.
//!
//! Contains [`UsageStats`] for per-session token accumulation, [`TurnState`]
//! for in-flight turn tracking, and helpers to detect the active model and
//! estimate dollar cost from token counts.

use std::time::Instant;

// ──────────────────────────────────────────────────────────────────────
// Usage / turn telemetry
// ──────────────────────────────────────────────────────────────────────

/// Token usage and timing accumulated across the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageStats {
    /// Most recent per-turn input tokens.
    pub input_tokens: u64,
    /// Most recent per-turn output tokens.
    pub output_tokens: u64,
    /// Cumulative input tokens across all turns.
    pub total_input: u64,
    /// Cumulative output tokens across all turns.
    pub total_output: u64,
    /// Most recent per-turn cache-hit tokens.
    pub cache_hit_tokens: u64,
    /// Most recent per-turn cache-miss tokens.
    pub cache_miss_tokens: u64,
    /// Cumulative cache-hit tokens across all turns.
    pub total_cache_hit: u64,
    /// Cumulative cache-miss tokens across all turns.
    pub total_cache_miss: u64,
    /// Cache-hit tokens summed over the in-progress / most recent turn.
    ///
    /// Reset to zero at the start of each turn via [`UsageStats::begin_turn`].
    /// The status bar uses this (not the cumulative totals) for the cache-hit
    /// rate, because the session totals trend toward ~100% as the cached
    /// prompt prefix is re-read on every step, drowning out the cold-start
    /// misses and making the figure useless.
    pub turn_cache_hit: u64,
    /// Cache-miss tokens summed over the in-progress / most recent turn.
    pub turn_cache_miss: u64,
    /// Most recent LLM round-trip latency, in milliseconds.
    pub last_latency_ms: u64,
}

impl UsageStats {
    /// Fold a `Usage` event into the stats. Treats incoming numbers as
    /// per-turn deltas and accumulates them into the running totals.
    pub fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self.total_input = self.total_input.saturating_add(input_tokens);
        self.total_output = self.total_output.saturating_add(output_tokens);
    }

    /// Fold a `Usage` event including cache fields into the stats.
    /// Treats incoming numbers as per-turn deltas and accumulates them
    /// into the running totals.
    pub fn record_with_cache(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_hit_tokens: u64,
        cache_miss_tokens: u64,
    ) {
        self.record(input_tokens, output_tokens);
        self.cache_hit_tokens = cache_hit_tokens;
        self.cache_miss_tokens = cache_miss_tokens;
        self.total_cache_hit = self.total_cache_hit.saturating_add(cache_hit_tokens);
        self.total_cache_miss = self.total_cache_miss.saturating_add(cache_miss_tokens);
        self.turn_cache_hit = self.turn_cache_hit.saturating_add(cache_hit_tokens);
        self.turn_cache_miss = self.turn_cache_miss.saturating_add(cache_miss_tokens);
    }

    /// Reset the per-turn cache counters. Called when a new turn starts so the
    /// status bar reports the cache-hit rate for the current turn rather than
    /// the whole session.
    pub fn begin_turn(&mut self) {
        self.turn_cache_hit = 0;
        self.turn_cache_miss = 0;
    }
}

/// State of the currently in-flight turn (if any).
#[derive(Clone, Debug, PartialEq)]
pub struct TurnState {
    pub running: bool,
    pub started_at: Option<Instant>,
    pub spinner_verb: &'static str,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            running: false,
            started_at: None,
            spinner_verb: "Thinking",
        }
    }
}

impl TurnState {
    pub fn start(&mut self) {
        self.running = true;
        self.started_at = Some(Instant::now());
        self.spinner_verb = "Thinking";
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.started_at = None;
        self.spinner_verb = "Thinking";
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cost estimation
// ──────────────────────────────────────────────────────────────────────

/// Return the model name to display in the status bar.
///
/// Delegates to `Config::from_env()` so the TUI shows the same model the
/// runtime will actually use — including the `provider.preset` chain added
/// for the preset-config goal. Without this, a user with
/// `provider.preset = "deepseek"` would see "claude-sonnet-4-6" in the
/// status bar while the agent talked to DeepSeek.
pub fn detect_model_name() -> String {
    crate::config::Config::from_env()
        .map(|c| c.model)
        .unwrap_or_else(|_| "gpt-4o-mini".to_string())
}

/// Saturating cast from u64 to u32: returns `u32::MAX` instead of wrapping.
///
/// `TokenUsage` fields are `u32` but session totals accumulate as `u64`.
/// Very long sessions (>4 billion tokens) are extremely rare in practice,
/// but saturating is safer than silent modular truncation.
fn saturating_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// Compute estimated cost in USD given accumulated tokens and cache stats.
/// Delegates to `pricing_for()` from the bundled provider catalog.
/// Uses `ModelPricing::cost_usd()` which applies cache-hit discount rates.
/// Returns `None` when the model has no pricing entry in `providers.toml`.
pub fn estimate_cost(
    model: &str,
    total_input: u64,
    total_output: u64,
    cache_hit: u64,
    cache_miss: u64,
) -> Option<f64> {
    let pricing = crate::llm::pricing_for(model)?;
    let usage = crate::llm::TokenUsage {
        prompt_tokens: saturating_u32(total_input),
        completion_tokens: saturating_u32(total_output),
        total_tokens: 0,
        cache_hit_tokens: saturating_u32(cache_hit),
        cache_miss_tokens: saturating_u32(cache_miss),
        reasoning_tokens: 0,
    };
    Some(pricing.cost_usd(usage))
}

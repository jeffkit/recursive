//! LLM pricing and retry policy.
//!
//! Contains the shared [`RetryPolicy`] struct used by all provider
//! implementations, the per-model [`ModelPricing`] struct, and lookup
//! helpers that consult the bundled `providers.toml` preset catalog.

use std::time::Duration;

use super::TokenUsage;

// ── Retry policy ────────────────────────────────────────────────────────────

/// Retry policy for transient LLM provider failures (network timeouts, 5xx).
///
/// Shared across all provider implementations to keep retry semantics
/// consistent. Each provider stores one instance in its struct and calls
/// [`RetryPolicy::backoff_for`] after every failed HTTP attempt.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Returns `Some(backoff)` if the caller should sleep-and-retry, or `None`
    /// to propagate the error. `attempt` is 0-indexed (0 = after the first failure).
    pub fn backoff_for(
        &self,
        attempt: usize,
        status: Option<u16>,
        is_network_error: bool,
    ) -> Option<Duration> {
        if attempt >= self.max_retries {
            return None;
        }
        let is_transient =
            is_network_error || status.is_some_and(|s| s == 429 || (500..600).contains(&s));
        if !is_transient {
            return None;
        }
        let backoff = self.initial_backoff * 2u32.saturating_pow(attempt as u32);
        Some(backoff.min(self.max_backoff))
    }
}

// ── Model pricing ────────────────────────────────────────────────────────────

/// Per-million-token pricing for one model. USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Price per million tokens for cache-hit prompts.
    /// Defaults to 10% of input rate (DeepSeek's known discount).
    pub cache_hit_input_per_million: f64,
}

impl ModelPricing {
    /// USD cost for the given usage at this pricing.
    pub fn cost_usd(&self, usage: TokenUsage) -> f64 {
        let in_cost = if usage.cache_hit_tokens > 0 {
            // Apply cache-hit discount: use cache_hit_input_per_million for cached tokens
            let cache_hit =
                usage.cache_hit_tokens as f64 * self.cache_hit_input_per_million / 1_000_000.0;
            // Use full rate for cache-miss tokens (which is prompt - cache_hit)
            // Note: cache_miss_tokens may not equal prompt - cache_hit due to rounding,
            // but the difference is negligible for billing purposes
            let cache_miss = usage.cache_miss_tokens as f64 * self.input_per_million / 1_000_000.0;
            cache_hit + cache_miss
        } else {
            (usage.prompt_tokens as f64) * self.input_per_million / 1_000_000.0
        };
        let total_output_tokens = usage
            .completion_tokens
            .saturating_add(usage.reasoning_tokens);
        let out_cost = total_output_tokens as f64 * self.output_per_million / 1_000_000.0;
        in_cost + out_cost
    }
}

// ── Lookup helpers ────────────────────────────────────────────────────────────

/// Returns the context window size in tokens for the given model.
///
/// The value is looked up from the bundled `providers.toml` preset catalog.
/// Unknown models (not listed in any preset) fall back to a conservative
/// 128 K token default — the minimum window common to all current-generation
/// frontier models.
pub fn context_window_tokens_for_model(model: &str) -> usize {
    use crate::providers::all_presets;
    for preset in all_presets() {
        for spec in &preset.models {
            if spec.name == model {
                return spec.context_window;
            }
        }
    }
    // Conservative fallback for models not listed in providers.toml.
    128_000
}

/// Compute the default compaction character-count threshold for a model.
///
/// Strategy (mirrors fake-cc `getAutoCompactThreshold`):
/// 1. Start from the model's context window in tokens.
/// 2. Reserve 20 000 tokens for the compaction summary output.
/// 3. Take 80 % of the remainder as the trigger point (leaves a comfortable
///    20 % buffer before the hard limit is hit).
/// 4. Convert tokens → characters using a conservative 4 chars/token ratio.
pub fn default_compact_threshold_chars(model: &str) -> usize {
    let context_tokens = context_window_tokens_for_model(model);
    let reserved_for_summary = 20_000_usize.min(context_tokens / 4);
    let effective_tokens = context_tokens.saturating_sub(reserved_for_summary);
    // 80 % of effective window, then 4 chars per token.
    (effective_tokens as f64 * 0.8 * 4.0) as usize
}

/// Returns pricing for a model by looking it up in the **effective** preset
/// catalog (remote cache + bundled + `providers.d/`), so per-token cost
/// reflects upstream catalog refreshes rather than just the compile-time
/// `providers.toml`. Returns `None` if the model is not listed or has no
/// pricing field.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let spec = crate::providers::find_model_pricing_effective(model)?;
    Some(ModelPricing {
        input_per_million: spec.input_per_million,
        output_per_million: spec.output_per_million,
        cache_hit_input_per_million: spec
            .cache_hit_input_per_million
            .unwrap_or(spec.input_per_million),
    })
}
